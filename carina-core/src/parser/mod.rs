//! Parser - Parse .crn files
//!
//! Convert DSL to AST using pest

mod config;

pub use config::{DecryptorFn, ProviderContext, ValidatorFn};

use indexmap::IndexMap;

use crate::eval_value::EvalValue;
use crate::resource::{Expr, LifecycleConfig, Resource, ResourceId, ResourceKind, Value};
use crate::schema::{
    validate_ipv4_address, validate_ipv4_cidr, validate_ipv6_address, validate_ipv6_cidr,
};
use crate::version_constraint::VersionConstraint;
use pest::Parser;
use pest_derive::Parser;
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Parser)]
#[grammar = "parser/carina.pest"]
struct CarinaParser;

/// Parse error
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Syntax error: {0}")]
    Syntax(#[from] pest::error::Error<Rule>),

    #[error("Invalid expression at line {line}: {message}")]
    InvalidExpression { line: usize, message: String },

    #[error("Undefined variable: {0}")]
    UndefinedVariable(String),

    #[error("Invalid resource type: {0}")]
    InvalidResourceType(String),

    #[error("Duplicate module definition: {0}")]
    DuplicateModule(String),

    #[error("Duplicate binding at line {line}: {name}")]
    DuplicateBinding { name: String, line: usize },

    #[error("{}", format_undefined_identifier(name, suggestion.as_deref(), in_scope))]
    UndefinedIdentifier {
        name: String,
        line: usize,
        /// Edit-distance close match among in-scope bindings, if any.
        /// Filled by the check site when a `suggest_similar_name` result is
        /// available; None for hand-constructed errors.
        suggestion: Option<String>,
        /// Concrete binding names in scope at the check site, sorted for
        /// deterministic rendering. Empty when no bindings have been
        /// declared at all.
        in_scope: Vec<String>,
    },

    #[error("Module not found: {0}")]
    ModuleNotFound(String),

    #[error("Internal parser error: expected {expected} in {context}")]
    InternalError { expected: String, context: String },

    #[error("Recursive function call detected: {0}")]
    RecursiveFunction(String),

    #[error("User-defined function error: {0}")]
    UserFunctionError(String),
}

/// Render the `UndefinedIdentifier` message. When a close match exists
/// (`suggestion`), lead with `Did you mean ...?`. Otherwise list the
/// concrete in-scope names so the reader learns what is available,
/// followed by the abstract list of binding kinds as a trailing aside.
/// See #2038.
fn format_undefined_identifier(
    name: &str,
    suggestion: Option<&str>,
    in_scope: &[String],
) -> String {
    if let Some(s) = suggestion {
        return format!("Undefined identifier `{}`. Did you mean `{}`?", name, s);
    }
    let kinds = "let / upstream_state / read / module / function / for / fn / arguments";
    if in_scope.is_empty() {
        format!(
            "Undefined identifier `{}`: no bindings are in scope ({})",
            name, kinds,
        )
    } else {
        format!(
            "Undefined identifier `{}`. In-scope names: {} ({})",
            name,
            in_scope.join(", "),
            kinds,
        )
    }
}

/// A structured warning emitted during parsing (non-fatal).
#[derive(Debug, Clone, PartialEq)]
pub struct ParseWarning {
    /// Full source path of the file this warning originated from
    /// (stamped by `config_loader` after parsing). `None` at parse time;
    /// always `Some` once the warning reaches CLI/LSP callers.
    pub file: Option<String>,
    pub line: usize,
    pub message: String,
}

/// Placeholder text for values that depend on an upstream apply.
pub const DEFERRED_UPSTREAM_PLACEHOLDER: &str = "(known after upstream apply)";
/// Placeholder reserved for map-binding key variables. Distinct from the
/// value-var placeholder so expansion can substitute each correctly.
pub(crate) const DEFERRED_UPSTREAM_KEY_PLACEHOLDER: &str = "(known after upstream apply: key)";
/// Placeholder reserved for indexed-binding index variables — distinct from
/// key and value placeholders so `for (i, x) in list` expansion can
/// substitute `i` with the integer index.
pub(crate) const DEFERRED_UPSTREAM_INDEX_PLACEHOLDER: &str = "(known after upstream apply: index)";

/// A for-expression whose iterable is unresolved (e.g., upstream_state not yet available).
/// Captures the structural shape of the loop body so the plan can show what
/// resources *would* be created once the iterable becomes available.
/// Also stores enough information to expand the loop later when the iterable
/// is loaded from upstream_state.
#[derive(Debug, Clone)]
pub struct DeferredForExpression {
    /// Full source path of the file this deferred expression originated
    /// from (stamped by `config_loader` after parsing).
    pub file: Option<String>,
    /// Source line number of the `for` keyword.
    pub line: usize,
    /// The for-expression header, e.g., `for account_id in orgs.accounts`.
    pub header: String,
    /// The provider-qualified resource type the loop body would produce (e.g., `awscc.sso.Assignment`).
    pub resource_type: String,
    /// Attribute template: key → value (concrete values are resolved;
    /// loop-bound variables remain as `ResourceRef` or placeholder strings).
    pub attributes: Vec<(String, Value)>,
    /// The binding address prefix for generated resources (e.g., `_for0`).
    pub binding_name: String,
    /// The iterable access path segments (e.g., `["orgs", "accounts"]`).
    pub iterable_binding: String,
    pub iterable_attr: String,
    /// Binding pattern — records the kind (Simple/Indexed/Map) so the expansion
    /// can verify the resolved iterable's shape matches and substitute the
    /// correct variable(s).
    pub binding: ForBinding,
    /// Template resource for expansion (the for body parsed with placeholders).
    pub template_resource: Resource,
}

/// Origin of a resource yielded by [`ParsedFile::iter_all_resources`].
///
/// `Direct` means the resource was declared at top-level and its iterable
/// (if any) resolved at parse time. `Deferred` means the resource is the
/// template body of a `for` expression whose iterable resolves later;
/// consumers that care about loop-variable placeholders need the
/// `DeferredForExpression` reference to filter them out.
#[derive(Debug, Clone, Copy)]
pub enum ResourceContext<'a> {
    Direct,
    Deferred(&'a DeferredForExpression),
}

/// Resource type path for typed references (e.g., aws.vpc, aws.security_group)
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResourceTypePath {
    /// Provider name (e.g., "aws")
    pub provider: String,
    /// Resource type (e.g., "vpc", "security_group")
    pub resource_type: String,
}

impl ResourceTypePath {
    pub fn new(provider: impl Into<String>, resource_type: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            resource_type: resource_type.into(),
        }
    }

    /// Parse from a dot-separated string (e.g., "aws.vpc" or "aws.security_group")
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() >= 2 {
            Some(Self {
                provider: parts[0].to_string(),
                resource_type: parts[1..].join("."),
            })
        } else {
            None
        }
    }
}

impl std::fmt::Display for ResourceTypePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.provider, self.resource_type)
    }
}

/// Type expression for arguments/attributes parameters
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TypeExpr {
    String,
    Bool,
    Int,
    Float,
    /// Schema type identified by name (e.g., "ipv4_cidr", "ipv4_address", "arn")
    Simple(std::string::String),
    List(Box<TypeExpr>),
    Map(Box<TypeExpr>),
    /// Reference to a resource type (e.g., aws.vpc)
    Ref(ResourceTypePath),
    /// Provider-defined schema type (e.g., awscc.ec2.VpcId, awscc.ec2.SubnetId)
    /// Distinguished from Ref by having a PascalCase final segment.
    SchemaType {
        /// Provider name (e.g., "awscc")
        provider: String,
        /// Service/namespace path (e.g., "ec2")
        path: String,
        /// Type name in PascalCase (e.g., "VpcId")
        type_name: String,
    },
    /// Structural record type: `struct { name: type, ... }`.
    ///
    /// Field order matches source order and participates in `PartialEq` —
    /// two struct types with the same fields in different order are not
    /// equal. A `Value::Map` satisfies a struct type when every field
    /// name appears as a key with a value that matches the field's type,
    /// with no extra keys.
    Struct {
        fields: Vec<(String, TypeExpr)>,
    },
}

impl std::fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeExpr::String => write!(f, "String"),
            TypeExpr::Bool => write!(f, "Bool"),
            TypeExpr::Int => write!(f, "Int"),
            TypeExpr::Float => write!(f, "Float"),
            TypeExpr::Simple(name) => write!(f, "{}", snake_to_pascal(name)),
            TypeExpr::List(inner) => write!(f, "list({})", inner),
            TypeExpr::Map(inner) => write!(f, "map({})", inner),
            TypeExpr::Ref(path) => write!(f, "{}", path),
            TypeExpr::SchemaType {
                provider,
                path,
                type_name,
            } => write!(f, "{}.{}.{}", provider, path, type_name),
            TypeExpr::Struct { fields } => {
                if fields.is_empty() {
                    write!(f, "struct {{}}")
                } else {
                    write!(f, "struct {{ ")?;
                    for (i, (name, ty)) in fields.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}: {}", name, ty)?;
                    }
                    write!(f, " }}")
                }
            }
        }
    }
}

/// Argument parameter definition (in `arguments { ... }` block)
#[derive(Debug, Clone)]
pub struct ArgumentParameter {
    pub name: String,
    pub type_expr: TypeExpr,
    pub default: Option<Value>,
    /// Optional description (from block form)
    pub description: Option<String>,
    /// Optional validation blocks (from block form). Multiple blocks are allowed.
    pub validations: Vec<ValidationBlock>,
}

/// A validate block: `validation { condition = <expr> error_message = "..." }`
#[derive(Debug, Clone)]
pub struct ValidationBlock {
    pub condition: ValidateExpr,
    pub error_message: Option<String>,
}

/// Comparison operator in a validate expression
#[derive(Debug, Clone, PartialEq)]
pub enum CompareOp {
    Gte,
    Lte,
    Gt,
    Lt,
    Eq,
    Ne,
}

/// Validate expression AST node
#[derive(Debug, Clone, PartialEq)]
pub enum ValidateExpr {
    /// Boolean literal
    Bool(bool),
    /// Integer literal
    Int(i64),
    /// Float literal
    Float(f64),
    /// String literal
    String(String),
    /// Variable reference (argument name)
    Var(String),
    /// Comparison: lhs op rhs
    Compare {
        lhs: Box<ValidateExpr>,
        op: CompareOp,
        rhs: Box<ValidateExpr>,
    },
    /// Logical AND
    And(Box<ValidateExpr>, Box<ValidateExpr>),
    /// Logical OR
    Or(Box<ValidateExpr>, Box<ValidateExpr>),
    /// Logical NOT
    Not(Box<ValidateExpr>),
    /// Function call (e.g., len(x))
    FunctionCall {
        name: String,
        args: Vec<ValidateExpr>,
    },
    /// Null literal
    Null,
}

/// A require block: `require <condition>, "error message"`
/// Used for cross-argument constraints at the module top level.
#[derive(Debug, Clone)]
pub struct RequireBlock {
    pub condition: ValidateExpr,
    pub error_message: String,
}

/// Attribute parameter definition (in `attributes { ... }` block)
#[derive(Debug, Clone)]
pub struct AttributeParameter {
    pub name: String,
    pub type_expr: Option<TypeExpr>,
    pub value: Option<Value>,
}

/// An export parameter in an `exports { }` block.
/// Publishes a named value for `upstream_state` consumers.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportParameter {
    pub name: String,
    pub type_expr: Option<TypeExpr>,
    pub value: Option<Value>,
}

/// State manipulation block (import, removed, moved)
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StateBlock {
    /// Import existing infrastructure into Carina management
    Import {
        /// Target resource address
        to: ResourceId,
        /// Cloud provider identifier (e.g., "vpc-0abc123def456")
        id: String,
    },
    /// Remove a resource from state without destroying it
    Removed {
        /// Resource address to remove from state
        from: ResourceId,
    },
    /// Rename/move a resource in state without destroy/recreate
    Moved {
        /// Old resource address
        from: ResourceId,
        /// New resource address
        to: ResourceId,
    },
}

/// Module `use` statement (previously `import`).
#[derive(Debug, Clone)]
pub struct UseStatement {
    pub path: String,
    pub alias: String,
}

/// Parameter for a user-defined function
#[derive(Debug, Clone)]
pub struct FnParam {
    pub name: String,
    pub param_type: Option<TypeExpr>,
    pub default: Option<Value>,
}

/// The body of a user-defined function: a value expression.
/// Functions are pure value transformations only.
#[derive(Debug, Clone)]
pub struct UserFunctionBody(pub Value);

/// User-defined pure function
#[derive(Debug, Clone)]
pub struct UserFunction {
    pub name: String,
    pub params: Vec<FnParam>,
    /// Optional return type annotation
    pub return_type: Option<TypeExpr>,
    /// Local let bindings inside the function body (name, expression)
    pub local_lets: Vec<(String, Value)>,
    /// The body of the function
    pub body: UserFunctionBody,
}

/// Module call (instantiation)
#[derive(Debug, Clone)]
pub struct ModuleCall {
    pub module_name: String,
    pub binding_name: Option<String>,
    pub arguments: HashMap<String, Value>,
}

/// Provider configuration
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub attributes: IndexMap<String, Value>,
    /// Default tags to apply to all resources that support tags.
    /// Extracted from `default_tags = { ... }` in the provider block.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub default_tags: IndexMap<String, Value>,
    /// Provider source (e.g., "github.com/carina-rs/carina-provider-awscc" or "file:///path/to/binary").
    /// Extracted from the provider block and not passed to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Provider version constraint (e.g., "~0.5.0", "^1.2.0").
    /// Extracted from the provider block and not passed to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<VersionConstraint>,
    /// Git revision (branch, tag, or commit SHA) to resolve the provider from CI artifacts.
    /// Mutually exclusive with `version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// Backend configuration for state storage
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BackendConfig {
    /// Backend type (e.g., "s3", "gcs", "local")
    pub backend_type: String,
    /// Backend-specific attributes
    pub attributes: HashMap<String, Value>,
}

/// Upstream state reference: `let <binding> = upstream_state { source = "<dir>" }`.
///
/// Declares a read-only reference to another Carina configuration's state.
///
/// `source` is a directory path. Carina itself is directory-scoped, so every
/// sibling `.crn` file in a project (or module) shares the same base
/// directory; the resolution rule is simply:
///
/// - Absolute paths are used as-is.
/// - Relative paths are resolved against the **enclosing project or module
///   directory** — the one passed to `carina validate` / `plan` / `apply`,
///   or the module directory containing the `upstream_state` declaration.
///
/// Which specific `.crn` file inside that directory declares the
/// `upstream_state` does not affect resolution. The upstream's backend and
/// state file are derived from the upstream configuration itself.
#[derive(Debug, Clone)]
pub struct UpstreamState {
    /// The binding name (e.g., "orgs")
    pub binding: String,
    /// Source directory (raw, unresolved path)
    pub source: std::path::PathBuf,
}

/// Path identifying a single attribute (possibly nested) on a specific resource.
///
/// Used to tag which attribute values were written as a quoted string literal
/// in the DSL (e.g. `target_type = "aaa"`) versus a bare identifier or
/// namespaced identifier (e.g. `target_type = AWS_ACCOUNT`). The parser drops
/// both of those origins into `Value::String`, so consumers that care about
/// the distinction (enum-variant diagnostics, primarily) consult this set.
///
/// `attribute_chain` uses the field name for struct / map descents and the
/// decimal string index for list descents, matching the path shape used by
/// nested-struct validation. The top-level attribute name (e.g.
/// `["target_type"]`) is the common case.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StringLiteralPath {
    pub resource_id: ResourceId,
    pub attribute_chain: Vec<String>,
}

/// Parse result
#[derive(Debug, Clone, Default)]
pub struct ParsedFile {
    pub providers: Vec<ProviderConfig>,
    pub resources: Vec<Resource>,
    pub variables: IndexMap<String, Value>,
    /// Module `use` statements
    pub uses: Vec<UseStatement>,
    /// Module calls (instantiations)
    pub module_calls: Vec<ModuleCall>,
    /// Top-level argument parameters (directory-based module style)
    pub arguments: Vec<ArgumentParameter>,
    /// Top-level attribute parameters (directory-based module style)
    pub attribute_params: Vec<AttributeParameter>,
    /// Top-level export parameters (published to upstream_state consumers)
    pub export_params: Vec<ExportParameter>,
    /// Backend configuration for state storage
    pub backend: Option<BackendConfig>,
    /// State manipulation blocks (import, removed, moved)
    pub state_blocks: Vec<StateBlock>,
    /// User-defined pure functions
    pub user_functions: HashMap<String, UserFunction>,
    /// Upstream state references (read-only views of other Carina configurations)
    pub upstream_states: Vec<UpstreamState>,
    /// Require blocks (cross-argument constraints)
    pub requires: Vec<RequireBlock>,
    /// Binding names that are structurally required (if/for/read expressions)
    /// and should not trigger unused-binding warnings.
    pub structural_bindings: HashSet<String>,
    /// Non-fatal warnings collected during parsing.
    pub warnings: Vec<ParseWarning>,
    /// For-expressions whose iterables are unresolved; displayed as deferred in plan.
    pub deferred_for_expressions: Vec<DeferredForExpression>,
    /// Attribute paths whose value was written as a quoted string literal in
    /// the source (as opposed to a bare or namespaced identifier). Parse-time
    /// information only; not persisted in state.
    pub string_literal_paths: HashSet<StringLiteralPath>,
}

impl ParsedFile {
    /// Iterate every resource reachable from the parsed file — both
    /// top-level `resources` and the `template_resource` of each deferred
    /// for-expression — tagged with its origin context.
    ///
    /// Per-attribute checkers (type, enum, required, ref validity, etc.)
    /// should prefer this over `self.resources.iter()` so they stay in sync
    /// with for-body code. See
    /// `docs/specs/2026-04-19-unify-resource-walk-design.md` for the
    /// rationale.
    pub fn iter_all_resources(&self) -> impl Iterator<Item = (ResourceContext<'_>, &Resource)> {
        self.resources
            .iter()
            .map(|r| (ResourceContext::Direct, r))
            .chain(
                self.deferred_for_expressions
                    .iter()
                    .map(|d| (ResourceContext::Deferred(d), &d.template_resource)),
            )
    }

    /// Find a resource by resource type and name attribute value
    pub fn find_resource_by_attr(
        &self,
        resource_type: &str,
        attr_name: &str,
        attr_value: &str,
    ) -> Option<&Resource> {
        self.resources.iter().find(|r| {
            r.id.resource_type == resource_type
                && matches!(r.get_attr(attr_name), Some(Value::String(n)) if n == attr_value)
        })
    }

    /// Print all collected warnings to stderr.
    pub fn print_warnings(&self) {
        for w in &self.warnings {
            let location = match &w.file {
                Some(f) => format!("{}:{}", f, w.line),
                None => format!("line {}", w.line),
            };
            eprintln!("  ⚠ {}: {}", location, w.message);
        }
    }

    /// Expand deferred for-expressions using loaded upstream_state bindings.
    ///
    /// For each deferred for-expression whose iterable can now be resolved
    /// from `remote_bindings`, expand the template into concrete resources
    /// and add them to `self.resources`. Resolved entries are removed from
    /// `deferred_for_expressions`; unresolved ones remain (with their
    /// warning preserved).
    pub fn expand_deferred_for_expressions(
        &mut self,
        remote_bindings: &HashMap<String, HashMap<String, Value>>,
    ) {
        let mut expanded_resources = Vec::new();
        let mut resolved_indices = Vec::new();
        // Indices where the iterable resolved but had the wrong shape. These
        // entries stay deferred (user must fix), but their parse-time "not yet
        // available" warning is replaced by the more specific shape-mismatch
        // warning collected in `new_warnings`.
        let mut mismatched_indices: Vec<usize> = Vec::new();
        let mut new_warnings: Vec<ParseWarning> = Vec::new();

        for (idx, deferred) in self.deferred_for_expressions.iter().enumerate() {
            // Look up the iterable value in remote_bindings
            let iterable = remote_bindings
                .get(&deferred.iterable_binding)
                .and_then(|attrs| attrs.get(&deferred.iterable_attr));

            let Some(iterable_value) = iterable else {
                continue;
            };

            match (&deferred.binding, iterable_value) {
                // Simple binding: only the value var is bound
                (ForBinding::Simple(_), Value::List(items)) => {
                    for (i, item) in items.iter().enumerate() {
                        let address = format!("{}[{}]", deferred.binding_name, i);
                        let mut resource = deferred.template_resource.clone();
                        resource.id.set_name(address.clone());
                        resource.binding = Some(address);
                        for (_k, expr) in resource.attributes.iter_mut() {
                            substitute_placeholder(&mut expr.0, None, None, item);
                        }
                        expanded_resources.push(resource);
                    }
                    resolved_indices.push(idx);
                }
                // Indexed binding: both index and value vars are bound
                (ForBinding::Indexed(_, _), Value::List(items)) => {
                    for (i, item) in items.iter().enumerate() {
                        let address = format!("{}[{}]", deferred.binding_name, i);
                        let mut resource = deferred.template_resource.clone();
                        resource.id.set_name(address.clone());
                        resource.binding = Some(address);
                        for (_k, expr) in resource.attributes.iter_mut() {
                            substitute_placeholder(&mut expr.0, Some(i as i64), None, item);
                        }
                        expanded_resources.push(resource);
                    }
                    resolved_indices.push(idx);
                }
                // Map binding expands over maps, substituting both key and value vars
                (ForBinding::Map(_, _), Value::Map(map)) => {
                    let mut keys: Vec<&String> = map.keys().collect();
                    keys.sort();
                    for key in keys {
                        let val = &map[key];
                        let address = format!("{}[\"{}\"]", deferred.binding_name, key);
                        let mut resource = deferred.template_resource.clone();
                        resource.id.set_name(address.clone());
                        resource.binding = Some(address);
                        for (_k, expr) in resource.attributes.iter_mut() {
                            substitute_placeholder(&mut expr.0, None, Some(key), val);
                        }
                        expanded_resources.push(resource);
                    }
                    resolved_indices.push(idx);
                }
                // Shape mismatch: replace the original "not yet available" warning
                // with a specific shape-mismatch warning, leave entry deferred.
                (ForBinding::Map(_, _), Value::List(_)) => {
                    mismatched_indices.push(idx);
                    new_warnings.push(ParseWarning {
                        file: deferred.file.clone(),
                        line: deferred.line,
                        message: format!(
                            "for binding expected map iterable but `{}.{}` resolved to a list. \
                             Fix either the upstream export shape or the downstream binding.",
                            deferred.iterable_binding, deferred.iterable_attr,
                        ),
                    });
                }
                (ForBinding::Simple(_), Value::Map(_))
                | (ForBinding::Indexed(_, _), Value::Map(_)) => {
                    mismatched_indices.push(idx);
                    new_warnings.push(ParseWarning {
                        file: deferred.file.clone(),
                        line: deferred.line,
                        message: format!(
                            "for binding expected list iterable but `{}.{}` resolved to a map. \
                             Fix either the upstream export shape or the downstream binding.",
                            deferred.iterable_binding, deferred.iterable_attr,
                        ),
                    });
                }
                _ => {
                    // Iterable is not a list or map — leave deferred
                }
            }
        }

        // Remove parse-time warnings for mismatched entries (the new
        // shape-mismatch warning replaces the generic "not yet available" one).
        for idx in &mismatched_indices {
            let deferred = &self.deferred_for_expressions[*idx];
            self.warnings
                .retain(|w| w.line != deferred.line || w.file != deferred.file);
        }

        // Remove resolved deferred entries (reverse order to preserve indices)
        for idx in resolved_indices.into_iter().rev() {
            // Also remove the corresponding warning
            let deferred = &self.deferred_for_expressions[idx];
            self.warnings
                .retain(|w| w.line != deferred.line || w.file != deferred.file);
            self.deferred_for_expressions.remove(idx);
        }

        self.resources.extend(expanded_resources);
        self.warnings.extend(new_warnings);
    }
}

/// Substitute deferred-for-expression placeholders in a Value tree.
///
/// Replaces `DEFERRED_UPSTREAM_PLACEHOLDER` with `value`. If `index` is
/// supplied (indexed-binding expansion), replaces
/// `DEFERRED_UPSTREAM_INDEX_PLACEHOLDER` with the integer index. If `key`
/// is supplied (map-binding expansion), replaces
/// `DEFERRED_UPSTREAM_KEY_PLACEHOLDER` with the key string. Recurses into
/// all compound Value variants so placeholders nested inside
/// interpolations / function calls / secrets are reached.
fn substitute_placeholder(v: &mut Value, index: Option<i64>, key: Option<&str>, value: &Value) {
    match v {
        Value::String(s) if s == DEFERRED_UPSTREAM_PLACEHOLDER => {
            *v = value.clone();
        }
        Value::String(s) if s == DEFERRED_UPSTREAM_KEY_PLACEHOLDER => {
            if let Some(k) = key {
                *v = Value::String(k.to_string());
            }
        }
        Value::String(s) if s == DEFERRED_UPSTREAM_INDEX_PLACEHOLDER => {
            if let Some(i) = index {
                *v = Value::Int(i);
            }
        }
        Value::List(items) => {
            for item in items.iter_mut() {
                substitute_placeholder(item, index, key, value);
            }
        }
        Value::Map(map) => {
            for val in map.values_mut() {
                substitute_placeholder(val, index, key, value);
            }
        }
        Value::FunctionCall { args, .. } => {
            for arg in args.iter_mut() {
                substitute_placeholder(arg, index, key, value);
            }
        }
        Value::Interpolation(parts) => {
            for part in parts.iter_mut() {
                if let crate::resource::InterpolationPart::Expr(inner) = part {
                    substitute_placeholder(inner, index, key, value);
                }
            }
        }
        Value::Secret(inner) => {
            substitute_placeholder(inner, index, key, value);
        }
        _ => {}
    }
}

/// Parse context (variable scope)
#[derive(Clone)]
struct ParseContext<'cfg> {
    /// Variables bound by `let` statements during parsing. Carries
    /// `EvalValue` rather than `Value` because partial applications
    /// (e.g. `let f = join("-")`) produce a closure that lives until a
    /// later pipe finishes the application. Lowered to `Value` at the
    /// end of `parse(...)`; an unfinished closure surfaces as a
    /// parse-time error.
    variables: IndexMap<String, EvalValue>,
    /// Resource bindings (binding_name -> Resource)
    resource_bindings: HashMap<String, Resource>,
    /// Imported modules (alias -> path)
    imported_modules: HashMap<String, String>,
    /// User-defined functions
    user_functions: HashMap<String, UserFunction>,
    /// Functions currently being evaluated (for recursion detection)
    evaluating_functions: Vec<String>,
    /// Parser configuration (decryptor, custom validators)
    config: &'cfg ProviderContext,
    /// Binding names from structurally-required expressions (if/for/read)
    structural_bindings: HashSet<String>,
    /// Upstream state bindings (binding_name -> UpstreamState)
    upstream_states: HashMap<String, UpstreamState>,
    /// Non-fatal warnings collected during parsing
    warnings: Vec<ParseWarning>,
    /// Deferred for-expressions collected during parsing
    deferred_for_expressions: Vec<DeferredForExpression>,
    /// Attribute paths whose RHS in the source is a plain quoted string
    /// literal. Populated by `parse_block_contents` while walking attribute
    /// values of a resource; handed back to `ParsedFile` at the top level.
    /// Wrapped in `Rc<RefCell<..>>` so that `ctx.clone()` (done for local
    /// block scopes) shares the same underlying set.
    string_literal_paths: std::rc::Rc<std::cell::RefCell<HashSet<StringLiteralPath>>>,
}

impl<'cfg> ParseContext<'cfg> {
    fn new(config: &'cfg ProviderContext) -> Self {
        Self {
            variables: IndexMap::new(),
            resource_bindings: HashMap::new(),
            imported_modules: HashMap::new(),
            user_functions: HashMap::new(),
            evaluating_functions: Vec::new(),
            config,
            structural_bindings: HashSet::new(),
            upstream_states: HashMap::new(),
            warnings: Vec::new(),
            deferred_for_expressions: Vec::new(),
            string_literal_paths: std::rc::Rc::new(std::cell::RefCell::new(HashSet::new())),
        }
    }

    fn record_string_literal(&self, path: StringLiteralPath) {
        self.string_literal_paths.borrow_mut().insert(path);
    }

    fn set_variable(&mut self, name: String, value: impl Into<EvalValue>) {
        self.variables.insert(name, value.into());
    }

    fn get_variable(&self, name: &str) -> Option<&EvalValue> {
        self.variables.get(name)
    }

    fn set_resource_binding(&mut self, name: String, resource: Resource) {
        self.resource_bindings.insert(name, resource);
    }

    fn is_resource_binding(&self, name: &str) -> bool {
        self.resource_bindings.contains_key(name)
    }
}

/// Helper to get the next element from a pest iterator, returning a ParseError on failure
fn next_pair<'a>(
    iter: &mut pest::iterators::Pairs<'a, Rule>,
    expected: &str,
    context: &str,
) -> Result<pest::iterators::Pair<'a, Rule>, ParseError> {
    iter.next().ok_or_else(|| ParseError::InternalError {
        expected: expected.to_string(),
        context: context.to_string(),
    })
}

/// Extract a key string from either an identifier or a quoted string pair.
/// For identifiers, returns the raw text. For strings, extracts the content
/// without quotes (supports both single-quoted and double-quoted strings).
fn extract_key_string(pair: pest::iterators::Pair<'_, Rule>) -> Result<String, ParseError> {
    match pair.as_rule() {
        Rule::identifier => Ok(pair.as_str().to_string()),
        Rule::string => {
            let inner = pair
                .into_inner()
                .next()
                .ok_or_else(|| ParseError::InternalError {
                    expected: "string content".to_string(),
                    context: "map/attribute key".to_string(),
                })?;
            match inner.as_rule() {
                Rule::single_quoted_string => {
                    // Extract content between quotes
                    let content = inner
                        .into_inner()
                        .next()
                        .map(|p| p.as_str().to_string())
                        .unwrap_or_default();
                    Ok(content)
                }
                Rule::double_quoted_string => {
                    let mut result = String::new();
                    for part in inner.into_inner() {
                        match part.as_rule() {
                            Rule::string_part => {
                                let inner_part = part.into_inner().next().unwrap();
                                match inner_part.as_rule() {
                                    Rule::string_literal => result.push_str(inner_part.as_str()),
                                    Rule::interpolation => {
                                        return Err(ParseError::InternalError {
                                            expected: "literal string".to_string(),
                                            context: "interpolation not supported in map keys"
                                                .to_string(),
                                        });
                                    }
                                    _ => result.push_str(inner_part.as_str()),
                                }
                            }
                            _ => result.push_str(part.as_str()),
                        }
                    }
                    Ok(result)
                }
                _ => Ok(inner.as_str().to_string()),
            }
        }
        _ => Ok(pair.as_str().to_string()),
    }
}

/// Helper to get the first inner pair from a pest pair
fn first_inner<'a>(
    pair: pest::iterators::Pair<'a, Rule>,
    expected: &str,
    context: &str,
) -> Result<pest::iterators::Pair<'a, Rule>, ParseError> {
    pair.into_inner()
        .next()
        .ok_or_else(|| ParseError::InternalError {
            expected: expected.to_string(),
            context: context.to_string(),
        })
}

/// Parse a .crn file with the given configuration.
///
/// The config allows injecting a decryptor function for `decrypt()` calls
/// and custom type validators from provider crates.
pub fn parse(input: &str, config: &ProviderContext) -> Result<ParsedFile, ParseError> {
    let preprocess_result =
        crate::heredoc::preprocess_heredocs(input).map_err(|e| ParseError::InvalidExpression {
            line: 0,
            message: e.to_string(),
        })?;
    let pairs = CarinaParser::parse(Rule::file, &preprocess_result.source)?;

    let mut ctx = ParseContext::new(config);
    let mut providers = Vec::new();
    let mut resources = Vec::new();
    let mut uses = Vec::new();
    let mut module_calls = Vec::new();
    let mut arguments = Vec::new();
    let mut attribute_params = Vec::new();
    let mut export_params = Vec::new();
    let mut backend = None;
    let mut state_blocks = Vec::new();
    let mut upstream_states: Vec<UpstreamState> = Vec::new();
    let mut requires = Vec::new();
    let mut anon_for_counter = 0usize;
    let mut anon_if_counter = 0usize;

    for pair in pairs {
        if pair.as_rule() == Rule::file {
            for inner in pair.into_inner() {
                if inner.as_rule() == Rule::statement {
                    for stmt in inner.into_inner() {
                        match stmt.as_rule() {
                            Rule::backend_block => {
                                backend = Some(parse_backend_block(stmt, &ctx)?);
                            }
                            Rule::provider_block => {
                                let provider = parse_provider_block(stmt, &ctx)?;
                                providers.push(provider);
                            }
                            Rule::arguments_block => {
                                let parsed_arguments =
                                    parse_arguments_block(stmt, config, &mut ctx.warnings)?;
                                for arg in &parsed_arguments {
                                    // Register argument names as lexical bindings so that
                                    // `vpc.vpc_id` resolves as ResourceRef and `cidr_block`
                                    // resolves as a variable reference during parsing.
                                    // No `arguments.` prefix needed.
                                    let placeholder_ref = Value::resource_ref(
                                        arg.name.clone(),
                                        String::new(),
                                        vec![],
                                    );
                                    ctx.set_variable(arg.name.clone(), placeholder_ref);
                                    let placeholder = Resource::new("_argument", &arg.name);
                                    ctx.set_resource_binding(arg.name.clone(), placeholder);
                                }
                                arguments.extend(parsed_arguments);
                            }
                            Rule::attributes_block => {
                                let parsed_attribute_params = {
                                    let warnings = std::mem::take(&mut ctx.warnings);
                                    let mut warnings = warnings;
                                    let result = parse_attributes_block(stmt, &ctx, &mut warnings);
                                    ctx.warnings = warnings;
                                    result?
                                };
                                attribute_params.extend(parsed_attribute_params);
                            }
                            Rule::exports_block => {
                                let parsed_export_params = {
                                    let warnings = std::mem::take(&mut ctx.warnings);
                                    let mut warnings = warnings;
                                    let result = parse_exports_block(stmt, &ctx, &mut warnings);
                                    ctx.warnings = warnings;
                                    result?
                                };
                                export_params.extend(parsed_export_params);
                            }
                            Rule::import_state_block => {
                                state_blocks.push(parse_import_state_block(stmt)?);
                            }
                            Rule::removed_block => {
                                state_blocks.push(parse_removed_block(stmt)?);
                            }
                            Rule::moved_block => {
                                state_blocks.push(parse_moved_block(stmt)?);
                            }
                            Rule::require_statement => {
                                requires.push(parse_require_statement(stmt)?);
                            }
                            Rule::for_expr => {
                                let iterable_name =
                                    extract_for_iterable_name(&stmt, anon_for_counter);
                                anon_for_counter += 1;
                                let (expanded_resources, expanded_module_calls) =
                                    parse_for_expr(stmt, &mut ctx, &iterable_name)?;
                                resources.extend(expanded_resources);
                                module_calls.extend(expanded_module_calls);
                            }
                            Rule::if_expr => {
                                let binding_name = format!("_if{}", anon_if_counter);
                                anon_if_counter += 1;
                                let (_value, expanded_resources, expanded_module_calls, _import) =
                                    parse_if_expr(stmt, &mut ctx, &binding_name)?;
                                resources.extend(expanded_resources);
                                module_calls.extend(expanded_module_calls);
                            }
                            Rule::fn_def => {
                                let user_fn = {
                                    let warnings = std::mem::take(&mut ctx.warnings);
                                    let mut warnings = warnings;
                                    let result = parse_fn_def(stmt, &ctx, &mut warnings);
                                    ctx.warnings = warnings;
                                    result?
                                };
                                let fn_name = user_fn.name.clone();
                                // Check for shadowing builtins
                                if crate::builtins::evaluate_builtin(&fn_name, &[]).is_ok()
                                    || crate::builtins::builtin_functions()
                                        .iter()
                                        .any(|f| f.name == fn_name)
                                {
                                    return Err(ParseError::UserFunctionError(format!(
                                        "function '{fn_name}' shadows a built-in function"
                                    )));
                                }
                                if ctx.user_functions.contains_key(&fn_name) {
                                    return Err(ParseError::UserFunctionError(format!(
                                        "duplicate function definition: '{fn_name}'"
                                    )));
                                }
                                ctx.user_functions.insert(fn_name, user_fn);
                            }
                            Rule::let_binding => {
                                let (line, _) = stmt.as_span().start_pos().line_col();
                                let (
                                    name,
                                    value,
                                    expanded_resources,
                                    expanded_module_calls,
                                    maybe_import,
                                    is_structural,
                                ) = parse_let_binding_extended(stmt, &mut ctx)?;
                                if is_structural {
                                    ctx.structural_bindings.insert(name.clone());
                                }
                                let is_discard = name == "_";
                                let is_upstream_state = ctx.upstream_states.contains_key(&name);
                                if !is_discard {
                                    if ctx.variables.contains_key(&name)
                                        || ctx.resource_bindings.contains_key(&name)
                                    {
                                        return Err(ParseError::DuplicateBinding { name, line });
                                    }
                                    if !is_upstream_state {
                                        ctx.set_variable(name.clone(), value);
                                    }
                                }
                                if !expanded_resources.is_empty() {
                                    if !is_discard {
                                        // Register the binding name as a resource binding
                                        // (use the first resource as placeholder)
                                        ctx.set_resource_binding(
                                            name.clone(),
                                            expanded_resources[0].clone(),
                                        );
                                    }
                                    resources.extend(expanded_resources);
                                }
                                if !expanded_module_calls.is_empty() {
                                    for mut call in expanded_module_calls {
                                        if call.binding_name.is_none() {
                                            call.binding_name = Some(name.clone());
                                        }
                                        module_calls.push(call);
                                    }
                                    if !is_discard {
                                        // Register as a resource binding so that
                                        // `name.attr` resolves as ResourceRef
                                        let placeholder = Resource::new("_module_binding", &name);
                                        ctx.set_resource_binding(name.clone(), placeholder);
                                    }
                                }
                                if is_upstream_state && !is_discard {
                                    let placeholder = Resource::new("_upstream_state", &name);
                                    ctx.set_resource_binding(name.clone(), placeholder);
                                    upstream_states.push(ctx.upstream_states[&name].clone());
                                }
                                if let Some(use_stmt) = maybe_import {
                                    ctx.imported_modules
                                        .insert(use_stmt.alias.clone(), use_stmt.path.clone());
                                    uses.push(use_stmt);
                                }
                            }
                            Rule::module_call => {
                                let call = parse_module_call(stmt, &ctx)?;
                                module_calls.push(call);
                            }
                            Rule::anonymous_resource => {
                                let resource = parse_anonymous_resource(stmt, &ctx)?;
                                resources.push(resource);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // Second pass: resolve forward references.
    // During parsing, unknown 2-part identifiers (e.g., vpc.vpc_id where vpc is
    // declared later) become String values like "vpc.vpc_id". Now that we have the
    // full binding set, convert matching ones to ResourceRef.
    resolve_forward_references(
        &ctx.resource_bindings,
        &mut resources,
        &mut attribute_params,
        &mut module_calls,
        &mut export_params,
    );

    // "Is every ResourceRef root declared somewhere?" is a semantic
    // question the per-file parse cannot answer: the referent may live
    // in a sibling `.crn`. The check runs post-merge via
    // `check_identifier_scope(&ParsedFile)` — wired into
    // `load_configuration_with_config`. See #2126 / #2138.

    let string_literal_paths = ctx.string_literal_paths.borrow().clone();

    // Lower the evaluator-internal `EvalValue` bindings to user-facing
    // `Value`. Closure bindings are dropped: they are evaluator-only
    // artifacts (partial applications produced by `let f = builtin(x)`
    // and consumed by a later pipe like `data |> f()`). After the parse
    // pass finishes, only fully-reduced `Value`s belong in
    // `ParsedFile.variables`; nothing downstream knows how to handle a
    // closure. Pipe / call paths read from `ctx.variables` directly via
    // `get_variable`, so the closure was already available where it
    // mattered.
    let variables: IndexMap<String, Value> = ctx
        .variables
        .into_iter()
        .filter_map(|(name, eval)| match eval.into_value() {
            Ok(v) => Some((name, v)),
            Err(_leak) => None,
        })
        .collect();

    Ok(ParsedFile {
        providers,
        resources,
        variables,
        uses,
        module_calls,
        arguments,
        attribute_params,
        export_params,
        backend,
        state_blocks,
        user_functions: ctx.user_functions,
        upstream_states,
        requires,
        structural_bindings: ctx.structural_bindings,
        warnings: ctx.warnings,
        deferred_for_expressions: ctx.deferred_for_expressions,
        string_literal_paths,
    })
}

/// Parse arguments block
fn parse_arguments_block(
    pair: pest::iterators::Pair<Rule>,
    config: &ProviderContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<Vec<ArgumentParameter>, ParseError> {
    let mut arguments = Vec::new();
    let ctx = ParseContext::new(config);

    for param in pair.into_inner() {
        if param.as_rule() == Rule::arguments_param {
            let mut param_inner = param.into_inner();
            let name = next_pair(&mut param_inner, "parameter name", "arguments block")?
                .as_str()
                .to_string();
            let type_expr = parse_type_expr(
                next_pair(&mut param_inner, "type expression", "arguments parameter")?,
                config,
                warnings,
            )?;

            // Check if the next element is a block form or simple default
            if let Some(next) = param_inner.next() {
                if next.as_rule() == Rule::arguments_param_block {
                    // Block form: parse description, default, validate blocks from attrs
                    let mut description = None;
                    let mut default = None;
                    let mut validations = Vec::new();
                    for attr in next.into_inner() {
                        if attr.as_rule() == Rule::arguments_param_attr {
                            let inner_attr =
                                first_inner(attr, "attribute", "arguments_param_attr")?;
                            match inner_attr.as_rule() {
                                Rule::arg_description_attr => {
                                    let string_pair =
                                        first_inner(inner_attr, "string", "arg_description_attr")?;
                                    let value = parse_string_value(string_pair, &ctx)?;
                                    if let Value::String(s) = value {
                                        description = Some(s);
                                    }
                                }
                                Rule::arg_default_attr => {
                                    let expr_pair =
                                        first_inner(inner_attr, "expression", "arg_default_attr")?;
                                    default = Some(parse_expression(expr_pair, &ctx)?);
                                }
                                Rule::arg_validation_block => {
                                    let mut rule = None;
                                    let mut error_msg = None;
                                    for vattr in inner_attr.into_inner() {
                                        if vattr.as_rule() == Rule::validation_block_attr {
                                            let inner_vattr = first_inner(
                                                vattr,
                                                "validation_block_attr",
                                                "validation_block_attr",
                                            )?;
                                            match inner_vattr.as_rule() {
                                                Rule::validation_condition_attr => {
                                                    let validate_pair = first_inner(
                                                        inner_vattr,
                                                        "validate_expr",
                                                        "validation_condition_attr",
                                                    )?;
                                                    rule =
                                                        Some(parse_validate_expr(validate_pair)?);
                                                }
                                                Rule::validation_error_message_attr => {
                                                    let string_pair = first_inner(
                                                        inner_vattr,
                                                        "string",
                                                        "validation_error_message_attr",
                                                    )?;
                                                    let value =
                                                        parse_string_value(string_pair, &ctx)?;
                                                    if let Value::String(s) = value {
                                                        error_msg = Some(s);
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    if let Some(condition) = rule {
                                        validations.push(ValidationBlock {
                                            condition,
                                            error_message: error_msg,
                                        });
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    arguments.push(ArgumentParameter {
                        name,
                        type_expr,
                        default,
                        description,
                        validations,
                    });
                } else {
                    // Simple form: the next element is the default expression
                    let default = Some(parse_expression(next, &ctx)?);
                    arguments.push(ArgumentParameter {
                        name,
                        type_expr,
                        default,
                        description: None,
                        validations: Vec::new(),
                    });
                }
            } else {
                // No default, no block
                arguments.push(ArgumentParameter {
                    name,
                    type_expr,
                    default: None,
                    description: None,
                    validations: Vec::new(),
                });
            }
        }
    }

    Ok(arguments)
}

/// Parse a require statement: `require <validate_expr>, "error message"`
fn parse_require_statement(pair: pest::iterators::Pair<Rule>) -> Result<RequireBlock, ParseError> {
    let mut inner = pair.into_inner();
    let condition_pair = next_pair(&mut inner, "validate_expr", "require statement")?;
    let condition = parse_validate_expr(condition_pair)?;
    let message_pair = next_pair(&mut inner, "string", "require statement")?;
    let raw = message_pair.as_str();
    let error_message = raw[1..raw.len() - 1].to_string();
    Ok(RequireBlock {
        condition,
        error_message,
    })
}

/// Parse a validate expression (boolean expression with comparisons and logical operators)
fn parse_validate_expr(pair: pest::iterators::Pair<Rule>) -> Result<ValidateExpr, ParseError> {
    match pair.as_rule() {
        Rule::validate_expr => {
            let inner = first_inner(pair, "validate_or_expr", "validate_expr")?;
            parse_validate_expr(inner)
        }
        Rule::validate_or_expr => {
            let mut inner = pair.into_inner();
            let first = next_pair(&mut inner, "validate_and_expr", "validate_or_expr")?;
            let mut result = parse_validate_expr(first)?;
            for next in inner {
                let right = parse_validate_expr(next)?;
                result = ValidateExpr::Or(Box::new(result), Box::new(right));
            }
            Ok(result)
        }
        Rule::validate_and_expr => {
            let mut inner = pair.into_inner();
            let first = next_pair(&mut inner, "validate_not_expr", "validate_and_expr")?;
            let mut result = parse_validate_expr(first)?;
            for next in inner {
                let right = parse_validate_expr(next)?;
                result = ValidateExpr::And(Box::new(result), Box::new(right));
            }
            Ok(result)
        }
        Rule::validate_not_expr => {
            let mut inner = pair.into_inner();
            let first = next_pair(&mut inner, "operand", "validate_not_expr")?;
            if first.as_rule() == Rule::validate_not_expr {
                // This is the "!" ~ validate_not_expr branch
                let operand = parse_validate_expr(first)?;
                Ok(ValidateExpr::Not(Box::new(operand)))
            } else {
                // This is the validate_comparison branch
                parse_validate_expr(first)
            }
        }
        Rule::validate_comparison => {
            let mut inner = pair.into_inner();
            let lhs_pair = next_pair(&mut inner, "validate_primary", "validate_comparison")?;
            let lhs = parse_validate_expr(lhs_pair)?;
            if let Some(op_pair) = inner.next() {
                let op = match op_pair.as_str() {
                    ">=" => CompareOp::Gte,
                    "<=" => CompareOp::Lte,
                    ">" => CompareOp::Gt,
                    "<" => CompareOp::Lt,
                    "==" => CompareOp::Eq,
                    "!=" => CompareOp::Ne,
                    other => {
                        return Err(ParseError::InvalidExpression {
                            line: 0,
                            message: format!("Unknown comparison operator: {}", other),
                        });
                    }
                };
                let rhs_pair =
                    next_pair(&mut inner, "validate_primary", "validate_comparison rhs")?;
                let rhs = parse_validate_expr(rhs_pair)?;
                Ok(ValidateExpr::Compare {
                    lhs: Box::new(lhs),
                    op,
                    rhs: Box::new(rhs),
                })
            } else {
                Ok(lhs)
            }
        }
        Rule::validate_primary => {
            let inner = first_inner(pair, "value", "validate_primary")?;
            parse_validate_expr(inner)
        }
        Rule::validate_function_call => {
            let mut inner = pair.into_inner();
            let name = next_pair(&mut inner, "function name", "validate_function_call")?
                .as_str()
                .to_string();
            let mut args = Vec::new();
            for arg_pair in inner {
                args.push(parse_validate_expr(arg_pair)?);
            }
            Ok(ValidateExpr::FunctionCall { name, args })
        }
        Rule::null_literal => Ok(ValidateExpr::Null),
        Rule::boolean => Ok(ValidateExpr::Bool(pair.as_str() == "true")),
        Rule::float => {
            let f: f64 = pair
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("Invalid float: {}", e),
                })?;
            Ok(ValidateExpr::Float(f))
        }
        Rule::number => {
            let n: i64 = pair
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("Invalid number: {}", e),
                })?;
            Ok(ValidateExpr::Int(n))
        }
        Rule::string => {
            // Simple string parsing (no interpolation support in validate expressions)
            let raw = pair.as_str();
            // Strip surrounding quotes
            let s = &raw[1..raw.len() - 1];
            Ok(ValidateExpr::String(s.to_string()))
        }
        Rule::variable_ref => {
            // Variable reference - just the identifier name
            // For validate expressions, we only support simple variable names
            let inner = first_inner(pair, "identifier", "variable_ref")?;
            Ok(ValidateExpr::Var(inner.as_str().to_string()))
        }
        other => Err(ParseError::InvalidExpression {
            line: 0,
            message: format!("Unexpected rule in validate expression: {:?}", other),
        }),
    }
}

/// Parse attributes block
fn parse_attributes_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<Vec<AttributeParameter>, ParseError> {
    let mut attribute_params = Vec::new();

    for param in pair.into_inner() {
        if param.as_rule() == Rule::attributes_param {
            let mut param_inner = param.into_inner();
            let name = next_pair(&mut param_inner, "parameter name", "attributes block")?
                .as_str()
                .to_string();

            // Check whether the next inner pair is a type_expr or an expression
            let next = next_pair(
                &mut param_inner,
                "type or expression",
                "attributes parameter",
            )?;
            let (type_expr, value) = if next.as_rule() == Rule::type_expr {
                // Has explicit type annotation: name: type = expr
                let type_expr = Some(parse_type_expr(next, ctx.config, warnings)?);
                let expr = next_pair(&mut param_inner, "value expression", "attributes parameter")?;
                let value = Some(parse_expression(expr, ctx)?);
                (type_expr, value)
            } else {
                // No type annotation: name = expr
                let value = Some(parse_expression(next, ctx)?);
                (None, value)
            };

            attribute_params.push(AttributeParameter {
                name,
                type_expr,
                value,
            });
        }
    }

    Ok(attribute_params)
}

fn parse_exports_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<Vec<ExportParameter>, ParseError> {
    let mut export_params = Vec::new();

    for param in pair.into_inner() {
        if param.as_rule() == Rule::exports_param {
            let mut param_inner = param.into_inner();
            let name = next_pair(&mut param_inner, "parameter name", "exports block")?
                .as_str()
                .to_string();

            let next = next_pair(&mut param_inner, "type or expression", "exports parameter")?;
            let (type_expr, value) = if next.as_rule() == Rule::type_expr {
                let type_expr = Some(parse_type_expr(next, ctx.config, warnings)?);
                let expr = next_pair(&mut param_inner, "value expression", "exports parameter")?;
                let value = Some(parse_expression(expr, ctx)?);
                (type_expr, value)
            } else {
                let value = Some(parse_expression(next, ctx)?);
                (None, value)
            };

            export_params.push(ExportParameter {
                name,
                type_expr,
                value,
            });
        }
    }

    Ok(export_params)
}

/// Parse type expression
fn parse_type_expr(
    pair: pest::iterators::Pair<Rule>,
    config: &ProviderContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<TypeExpr, ParseError> {
    let _ = warnings;
    let inner = first_inner(pair, "type", "type expression")?;
    match inner.as_rule() {
        Rule::type_simple => {
            let line = inner.as_span().start_pos().line_col().0;
            let text = inner.as_str();
            match text {
                "String" => Ok(TypeExpr::String),
                "Bool" => Ok(TypeExpr::Bool),
                "Int" => Ok(TypeExpr::Int),
                "Float" => Ok(TypeExpr::Float),
                // Phase C: the transition window for snake_case primitives
                // and custom types has closed. The parser accepts only
                // PascalCase type names (naming-conventions design D1).
                "string" | "bool" | "int" | "float" => Err(ParseError::InvalidExpression {
                    line,
                    message: format!(
                        "unknown type '{text}'; primitive types are PascalCase — use '{}' instead",
                        snake_to_pascal(text)
                    ),
                }),
                other if other.chars().next().is_some_and(|c| c.is_ascii_uppercase()) => {
                    Ok(TypeExpr::Simple(pascal_to_snake(other)))
                }
                other => Err(ParseError::InvalidExpression {
                    line,
                    message: format!(
                        "unknown type '{other}'; custom types are PascalCase — use '{}' instead",
                        snake_to_pascal(other)
                    ),
                }),
            }
        }
        Rule::type_generic => {
            // Get the full string representation to determine if it's list or map
            let full_str = inner.as_str();
            let is_list = full_str.starts_with("list");

            // Get the inner type expression
            let mut generic_inner = inner.into_inner();
            let inner_type = parse_type_expr(
                next_pair(&mut generic_inner, "inner type", "generic type expression")?,
                config,
                warnings,
            )?;

            if is_list {
                Ok(TypeExpr::List(Box::new(inner_type)))
            } else {
                Ok(TypeExpr::Map(Box::new(inner_type)))
            }
        }
        Rule::type_ref => {
            // Parse resource_type_path directly (e.g., aws.vpc or awscc.ec2.VpcId)
            let mut ref_inner = inner.into_inner();
            let path_str = next_pair(&mut ref_inner, "resource type path", "type ref")?.as_str();
            let parts: Vec<&str> = path_str.split('.').collect();

            // A 3+ segment path with a PascalCase final segment is ambiguous:
            // `aws.ec2.Vpc` is a resource kind (Ref), `awscc.ec2.VpcId` is a
            // schema type. Disambiguate by asking the provider context:
            // registered schema types become SchemaType, everything else
            // falls back to Ref.
            let has_pascal_tail = parts.len() >= 3
                && parts
                    .last()
                    .is_some_and(|s| s.starts_with(|c: char| c.is_uppercase()));
            if has_pascal_tail {
                let provider = parts[0];
                let path = parts[1..parts.len() - 1].join(".");
                let type_name = parts.last().unwrap();
                if config.is_schema_type(provider, &path, type_name) {
                    return Ok(TypeExpr::SchemaType {
                        provider: provider.to_string(),
                        path,
                        type_name: type_name.to_string(),
                    });
                }
            }
            let path = ResourceTypePath::parse(path_str).ok_or_else(|| {
                ParseError::InvalidResourceType(format!("Invalid resource type path: {}", path_str))
            })?;
            Ok(TypeExpr::Ref(path))
        }
        Rule::type_struct => {
            let mut fields: Vec<(String, TypeExpr)> = Vec::new();
            for child in inner.into_inner() {
                if child.as_rule() != Rule::struct_field_list {
                    continue;
                }
                for field_pair in child.into_inner() {
                    if field_pair.as_rule() != Rule::struct_field {
                        continue;
                    }
                    let mut field_inner = field_pair.into_inner();
                    let name = next_pair(&mut field_inner, "field name", "struct field")?
                        .as_str()
                        .to_string();
                    let ty = parse_type_expr(
                        next_pair(&mut field_inner, "field type", "struct field")?,
                        config,
                        warnings,
                    )?;
                    if fields.iter().any(|(existing, _)| existing == &name) {
                        return Err(ParseError::InvalidResourceType(format!(
                            "struct has duplicate field name '{name}'"
                        )));
                    }
                    fields.push((name, ty));
                }
            }
            Ok(TypeExpr::Struct { fields })
        }
        _ => Ok(TypeExpr::String),
    }
}

/// Parse import expression (RHS of `let name = use { source = "path" }`)
fn parse_use_expr(
    pair: pest::iterators::Pair<Rule>,
    binding_name: &str,
    ctx: &ParseContext,
) -> Result<UseStatement, ParseError> {
    let span = pair.as_span();
    let line = span.start_pos().line_col().0;
    let mut source: Option<String> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() != Rule::attribute {
            continue;
        }
        let attr_span = attr.as_span();
        let attr_line = attr_span.start_pos().line_col().0;
        let mut attr_inner = attr.into_inner();
        let key = next_pair(&mut attr_inner, "attribute name", "use expression")?
            .as_str()
            .to_string();
        let value_pair = next_pair(&mut attr_inner, "attribute value", "use expression")?;
        if key != "source" {
            return Err(ParseError::InvalidExpression {
                line: attr_line,
                message: format!("`use` block only accepts a `source` attribute, got `{key}`"),
            });
        }
        let value = parse_expression(value_pair, ctx)?;
        let Value::String(path) = value else {
            return Err(ParseError::InvalidExpression {
                line: attr_line,
                message: "`use` block `source` must be a string literal".to_string(),
            });
        };
        if source.is_some() {
            return Err(ParseError::InvalidExpression {
                line: attr_line,
                message: "`use` block has more than one `source` attribute".to_string(),
            });
        }
        source = Some(path);
    }

    let path = source.ok_or_else(|| ParseError::InvalidExpression {
        line,
        message: "`use` block must have a `source` attribute".to_string(),
    })?;

    Ok(UseStatement {
        path,
        alias: binding_name.to_string(),
    })
}

/// Parse module call
fn parse_module_call(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ModuleCall, ParseError> {
    let span = pair.as_span();
    let mut inner = pair.into_inner();
    let module_name = next_pair(&mut inner, "module name", "module call")?
        .as_str()
        .to_string();

    if module_name == "remote_state" {
        return Err(ParseError::InvalidExpression {
            line: span.start_pos().line_col().0,
            message: "`remote_state` has been replaced by `let <binding> = upstream_state { source = \"...\" }`".to_string(),
        });
    }

    let mut arguments = HashMap::new();
    for arg in inner {
        if arg.as_rule() == Rule::module_call_arg {
            let mut arg_inner = arg.into_inner();
            let key = next_pair(&mut arg_inner, "argument name", "module call argument")?
                .as_str()
                .to_string();
            let value = parse_expression(
                next_pair(&mut arg_inner, "argument value", "module call argument")?,
                ctx,
            )?;
            arguments.insert(key, value);
        }
    }

    Ok(ModuleCall {
        module_name,
        binding_name: None,
        arguments,
    })
}

/// Result of parsing the RHS of a let binding: (value, resources, module_calls, use_statement)
/// Tuple returned by the let-binding parser. The RHS is `EvalValue`
/// rather than `Value` so partial applications (closures) can survive
/// until a later pipe finishes them; the surrounding parse pass lowers
/// each binding to `Value` at the end of `parse(...)`.
type LetBindingRhs = (
    EvalValue,
    Vec<Resource>,
    Vec<ModuleCall>,
    Option<UseStatement>,
);

/// Extended parse_let_binding that also handles module calls, imports, and for expressions.
///
/// Returns `(name, value, resources, module_calls, import, is_structural)`.
/// `is_structural` is true when the RHS is an if/for/read expression, meaning the
/// `let` binding is structurally required and should not trigger unused-binding warnings.
#[allow(clippy::type_complexity)]
fn parse_let_binding_extended(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
) -> Result<
    (
        String,
        EvalValue,
        Vec<Resource>,
        Vec<ModuleCall>,
        Option<UseStatement>,
        bool,
    ),
    ParseError,
> {
    let mut inner = pair.into_inner();
    let name = next_pair(&mut inner, "binding name", "let binding")?
        .as_str()
        .to_string();
    let rhs_pair = next_pair(&mut inner, "expression", "let binding")?;

    // `use` is the only meta-statement allowed as a let-binding RHS. The
    // grammar permits it here and only here (see carina.pest); everywhere
    // else it is a parse error.
    if rhs_pair.as_rule() == Rule::use_expr {
        let use_stmt = parse_use_expr(rhs_pair, &name, ctx)?;
        let value = Value::String(format!("${{use:{}}}", use_stmt.path));
        return Ok((
            name,
            EvalValue::from_value(value),
            vec![],
            vec![],
            Some(use_stmt),
            false,
        ));
    }

    // Detect if the RHS is a structurally-required expression (if/for/read)
    let is_structural = detect_structural_rhs(&rhs_pair);

    // Check if it's a module call, resource expression, or for expression
    let (value, expanded_resources, module_calls, maybe_import) =
        parse_expression_with_resource_or_module(rhs_pair, ctx, &name)?;

    Ok((
        name,
        value,
        expanded_resources,
        module_calls,
        maybe_import,
        is_structural,
    ))
}

/// Detect if an expression pair's innermost primary is an if/for/read/upstream_state expression.
fn detect_structural_rhs(pair: &pest::iterators::Pair<Rule>) -> bool {
    // Walk into expression -> pipe_expr -> compose_expr -> primary -> inner
    fn find_inner_rule(pair: &pest::iterators::Pair<Rule>) -> Option<Rule> {
        let inner = pair.clone().into_inner().next()?;
        match inner.as_rule() {
            Rule::if_expr
            | Rule::for_expr
            | Rule::read_resource_expr
            | Rule::upstream_state_expr => Some(inner.as_rule()),
            Rule::pipe_expr | Rule::compose_expr | Rule::coalesce_expr | Rule::expression => {
                find_inner_rule(&inner)
            }
            Rule::primary => {
                let primary_inner = inner.into_inner().next()?;
                match primary_inner.as_rule() {
                    Rule::if_expr
                    | Rule::for_expr
                    | Rule::read_resource_expr
                    | Rule::upstream_state_expr => Some(primary_inner.as_rule()),
                    _ => None,
                }
            }
            _ => None,
        }
    }
    find_inner_rule(pair).is_some()
}

/// Parse expression with potential resource, module call, or import.
fn parse_expression_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let coalesce = first_inner(pair, "expression", "expression with resource or module")?;
    let pipe = first_inner(coalesce, "pipe expression", "coalesce expression")?;
    parse_pipe_expr_with_resource_or_module(pipe, ctx, binding_name)
}

fn parse_pipe_expr_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let mut inner = pair.into_inner();
    let compose_pair = next_pair(&mut inner, "compose expression", "pipe expression")?;

    // Unwrap compose_expr: get its inner pairs
    let mut compose_inner = compose_pair.into_inner();
    let primary = next_pair(
        &mut compose_inner,
        "primary expression",
        "compose expression",
    )?;
    let (mut value, expanded_resources, module_calls, maybe_import) =
        parse_primary_with_resource_or_module(primary, ctx, binding_name)?;

    // Handle >> composition within the compose_expr
    let compose_rhs: Vec<_> = compose_inner.collect();
    if !compose_rhs.is_empty() {
        // Process the compose chain
        for rhs_pair in compose_rhs {
            let rhs = parse_primary_eval(rhs_pair, ctx)?;

            if !value.is_closure() {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: format!(
                        "left side of >> must be a Closure (partially applied function), got {}",
                        eval_type_name(&value)
                    ),
                });
            }
            if !rhs.is_closure() {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: format!(
                        "right side of >> must be a Closure (partially applied function), got {}",
                        eval_type_name(&rhs)
                    ),
                });
            }

            let functions = if let EvalValue::Closure {
                name,
                captured_args,
                ..
            } = &value
                && name == "__compose__"
            {
                let mut fns = captured_args.clone();
                fns.push(rhs);
                fns
            } else {
                vec![value, rhs]
            };

            value = EvalValue::closure("__compose__", functions, 1);
        }
    }

    // Desugar pipe: `x |> f(args)` becomes `f(x, args)`
    for func_call_pair in inner {
        let mut fc_inner = func_call_pair.into_inner();
        let func_name = next_pair(&mut fc_inner, "function name", "pipe function call")?
            .as_str()
            .to_string();
        let extra_args: Result<Vec<Value>, ParseError> =
            fc_inner.map(|arg| parse_expression(arg, ctx)).collect();
        let extra_args = extra_args?;

        // Lower the running pipe value to a `Value` for the builtin
        // dispatch path (which expects fully-reduced data arguments).
        // Closures are handled separately just below.
        let pipe_value_for_args = match &value {
            EvalValue::User(v) => Some(v.clone()),
            EvalValue::Closure { .. } => None,
        };

        // Check if the pipe target is a Closure variable
        if let Some(EvalValue::Closure {
            name: fn_name,
            captured_args,
            remaining_arity,
        }) = ctx.get_variable(&func_name)
        {
            // Build closure-application args. The pipe value (`x` in
            // `x |> f`) goes as the last argument; we keep it as
            // EvalValue so a chained closure can pipe through.
            let mut all_args: Vec<EvalValue> = extra_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            all_args.push(value.clone());
            if extra_args.iter().all(is_static_value)
                && pipe_value_for_args.as_ref().is_some_and(is_static_value)
            {
                value = crate::builtins::apply_closure_with_config(
                    fn_name,
                    captured_args,
                    *remaining_arity,
                    &all_args,
                    ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: e,
                })?;
                continue;
            }
        }

        // Build args for the non-closure dispatch path: at this point
        // we need a `Vec<Value>`, so the running pipe value must be a
        // user-facing value (not a closure).
        let pipe_value = match pipe_value_for_args {
            Some(v) => v,
            None => {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: format!(
                        "cannot pipe a closure into '{}' — finish the partial application first",
                        func_name
                    ),
                });
            }
        };
        let mut args = extra_args;
        args.push(pipe_value);

        // Eagerly evaluate partial application for builtin pipe targets
        if let Some(arity) = crate::builtins::builtin_arity(&func_name)
            && args.len() < arity
            && args.iter().all(is_static_value)
        {
            let eval_args: Vec<EvalValue> =
                args.iter().cloned().map(EvalValue::from_value).collect();
            value =
                crate::builtins::evaluate_builtin_with_config(&func_name, &eval_args, ctx.config)
                    .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("{}(): {}", func_name, e),
                })?;
            continue;
        }

        value = EvalValue::from_value(Value::FunctionCall {
            name: func_name,
            args,
        });
    }

    Ok((value, expanded_resources, module_calls, maybe_import))
}

fn parse_primary_with_resource_or_module(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let inner = first_inner(pair, "value", "primary expression")?;

    match inner.as_rule() {
        Rule::read_resource_expr => {
            let resource = parse_read_resource_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((
                EvalValue::from_value(ref_value),
                vec![resource],
                vec![],
                None,
            ))
        }
        Rule::upstream_state_expr => {
            let (line, _) = inner.as_span().start_pos().line_col();
            let us = parse_upstream_state_expr(inner, binding_name)?;
            if ctx.upstream_states.contains_key(&us.binding) {
                return Err(ParseError::DuplicateBinding {
                    name: us.binding,
                    line,
                });
            }
            ctx.upstream_states.insert(us.binding.clone(), us);
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((EvalValue::from_value(ref_value), vec![], vec![], None))
        }
        Rule::resource_expr => {
            let resource = parse_resource_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((
                EvalValue::from_value(ref_value),
                vec![resource],
                vec![],
                None,
            ))
        }
        Rule::for_expr => {
            let (resources, module_calls) = parse_for_expr(inner, ctx, binding_name)?;
            let ref_value = Value::String(format!("${{for:{}}}", binding_name));
            Ok((
                EvalValue::from_value(ref_value),
                resources,
                module_calls,
                None,
            ))
        }
        Rule::if_expr => {
            let (value, resources, module_calls, import) = parse_if_expr(inner, ctx, binding_name)?;
            Ok((value, resources, module_calls, import))
        }
        Rule::module_call => {
            let call = parse_module_call(inner, ctx)?;
            let value = Value::String(format!("${{module:{}}}", call.module_name));
            Ok((EvalValue::from_value(value), vec![], vec![call], None))
        }
        Rule::function_call => {
            let value = parse_primary_eval(inner, ctx)?;
            Ok((value, vec![], vec![], None))
        }
        _ => {
            let value = parse_primary_eval(inner, ctx)?;
            Ok((value, vec![], vec![], None))
        }
    }
}

/// Binding pattern for a for expression
#[derive(Debug, Clone, PartialEq)]
pub enum ForBinding {
    /// Simple: `for x in ...`
    Simple(String),
    /// Indexed: `for (i, x) in ...`
    Indexed(String, String),
    /// Map: `for k, v in ...`
    Map(String, String),
}

impl ForBinding {
    /// Every binding name introduced by this pattern, in declaration order.
    pub fn names(&self) -> Vec<&str> {
        match self {
            ForBinding::Simple(a) => vec![a.as_str()],
            ForBinding::Indexed(a, b) | ForBinding::Map(a, b) => vec![a.as_str(), b.as_str()],
        }
    }
}

/// Whether `name` appears in `text` as a whole identifier (not a substring
/// of a longer identifier). An identifier is bounded by anything that is
/// not ASCII alphanumeric or `_`.
fn identifier_appears_in(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let name_bytes = name.as_bytes();
    let is_id_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_';

    let mut i = 0;
    while i + name_bytes.len() <= bytes.len() {
        if &bytes[i..i + name_bytes.len()] == name_bytes {
            let before_ok = i == 0 || !is_id_char(bytes[i - 1]);
            let after_idx = i + name_bytes.len();
            let after_ok = after_idx == bytes.len() || !is_id_char(bytes[after_idx]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Whether `s` is shaped like a bare Carina identifier — the first byte is
/// `A-Za-z_` and the rest are `A-Za-z0-9_`. Used to recover from the
/// parser's collapse of unresolved identifiers into `Value::String(s)`
/// when we need to decide whether to render an error as "identifier" vs
/// "string literal". See #2101.
fn is_bare_identifier(s: &str) -> bool {
    let mut bytes = s.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Result of parsing a for expression body: either a resource or a module call
enum ForBodyResult {
    Resource(Box<Resource>),
    ModuleCall(ModuleCall),
}

/// Parse a for expression and expand it into individual resources and/or module calls.
///
/// `for x in list { resource_expr }` expands to resources with addresses like
/// `binding[0]`, `binding[1]`, etc.
///
/// Extract a binding name from a for-expression's iterable.
///
/// For `for x in orgs.accounts { ... }`, returns `_accounts`.
/// For non-variable iterables (lists, function calls), falls back to `_for{N}`.
fn extract_for_iterable_name(pair: &pest::iterators::Pair<Rule>, counter: usize) -> String {
    let fallback = format!("_for{}", counter);
    let mut inner = pair.clone().into_inner();
    // Skip for_binding, take for_iterable
    let iterable_pair = inner.nth(1);
    let Some(iterable) = iterable_pair else {
        return fallback;
    };
    // Only extract name from variable_ref iterables
    let first_child = iterable.into_inner().next();
    let Some(child) = first_child else {
        return fallback;
    };
    if child.as_rule() != Rule::variable_ref {
        return fallback;
    }
    // Take the last segment of the dotted path (e.g., "accounts" from "orgs.accounts")
    let text = child.as_str().trim();
    let last_segment = text.rsplit('.').next().unwrap_or(text);
    format!("_{}", last_segment)
}

/// `for k, v in map { resource_expr }` expands to resources with addresses like
/// `binding["key1"]`, `binding["key2"]`, etc.
///
/// When the body is a module call, each iteration produces a module call with
/// a binding name like `binding[0]` or `binding["key"]`.
fn parse_for_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &str,
) -> Result<(Vec<Resource>, Vec<ModuleCall>), ParseError> {
    let for_line = pair.as_span().start_pos().line_col().0;
    let mut inner = pair.into_inner();

    // Parse the binding pattern
    let binding_pair = next_pair(&mut inner, "for binding", "for expression")?;
    let binding = parse_for_binding(binding_pair)?;

    // Parse the iterable expression
    let iterable_pair = next_pair(&mut inner, "iterable", "for expression")?;
    let iterable = parse_for_iterable(iterable_pair, ctx)?;

    // Parse the body (we'll re-parse it for each iteration)
    let body_pair = next_pair(&mut inner, "body", "for expression")?;

    // Warn on loop variables never referenced in the body. `_` is a discard
    // marker and is skipped. The check is a scan over the body's raw text
    // for the binding name as a whole identifier — good enough because the
    // grammar already guarantees identifiers appear at token boundaries.
    let body_text = body_pair.as_str();
    for var in binding.names() {
        if var == "_" || identifier_appears_in(body_text, var) {
            continue;
        }
        ctx.warnings.push(ParseWarning {
            file: None,
            line: for_line,
            message: format!(
                "for-loop binding '{}' is unused. Rename to '_' to suppress this warning.",
                var
            ),
        });
    }

    let mut resources = Vec::new();
    let mut module_calls = Vec::new();

    let collect = |result: ForBodyResult,
                   resources: &mut Vec<Resource>,
                   module_calls: &mut Vec<ModuleCall>| {
        match result {
            ForBodyResult::Resource(r) => resources.push(*r),
            ForBodyResult::ModuleCall(c) => module_calls.push(c),
        }
    };

    // Helper: only register non-discard bindings so `_` is never addressable.
    let bind = |c: &mut ParseContext, name: &str, v: Value| {
        if name != "_" {
            c.set_variable(name.to_string(), v);
        }
    };

    // Expand based on iterable type
    match (&binding, &iterable) {
        (ForBinding::Simple(var), Value::List(items)) => {
            for (i, item) in items.iter().enumerate() {
                let address = format!("{}[{}]", binding_name, i);
                let mut iter_ctx = ctx.clone();
                bind(&mut iter_ctx, var, item.clone());
                let result = parse_for_body(body_pair.clone(), &iter_ctx, &address)?;
                collect(result, &mut resources, &mut module_calls);
            }
        }
        (ForBinding::Indexed(idx_var, val_var), Value::List(items)) => {
            for (i, item) in items.iter().enumerate() {
                let address = format!("{}[{}]", binding_name, i);
                let mut iter_ctx = ctx.clone();
                bind(&mut iter_ctx, idx_var, Value::Int(i as i64));
                bind(&mut iter_ctx, val_var, item.clone());
                let result = parse_for_body(body_pair.clone(), &iter_ctx, &address)?;
                collect(result, &mut resources, &mut module_calls);
            }
        }
        (ForBinding::Map(key_var, val_var), Value::Map(map)) => {
            // Sort keys for deterministic output
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                let val = &map[key];
                let address = format!("{}[\"{}\"]", binding_name, key);
                let mut iter_ctx = ctx.clone();
                bind(&mut iter_ctx, key_var, Value::String(key.clone()));
                bind(&mut iter_ctx, val_var, val.clone());
                let result = parse_for_body(body_pair.clone(), &iter_ctx, &address)?;
                collect(result, &mut resources, &mut module_calls);
            }
        }
        // Unresolved reference — defer expansion to plan/apply when the
        // upstream values are loaded. Field validity is checked statically
        // by `upstream_exports::check_upstream_state_field_references`; for
        // a valid field the deferral is an implementation detail the user
        // doesn't need to hear about at validate time.
        (_, Value::ResourceRef { path }) => {
            // Build the for-expression header string
            let header = match &binding {
                ForBinding::Simple(var) => {
                    format!("for {} in {}", var, path.to_dot_string())
                }
                ForBinding::Indexed(idx, val) => {
                    format!("for ({}, {}) in {}", idx, val, path.to_dot_string())
                }
                ForBinding::Map(k, v) => {
                    format!("for {}, {} in {}", k, v, path.to_dot_string())
                }
            };

            // Try to parse the body once with placeholder values for the loop
            // variable(s) to extract the resource type and attribute template.
            let mut template_ctx = ctx.clone();
            let placeholder = || Value::String(DEFERRED_UPSTREAM_PLACEHOLDER.to_string());
            let bind = |c: &mut ParseContext, name: &str, v: Value| {
                if name != "_" {
                    c.set_variable(name.to_string(), v);
                }
            };
            match &binding {
                ForBinding::Simple(var) => {
                    bind(&mut template_ctx, var, placeholder());
                }
                ForBinding::Indexed(idx, val) => {
                    bind(
                        &mut template_ctx,
                        idx,
                        Value::String(DEFERRED_UPSTREAM_INDEX_PLACEHOLDER.to_string()),
                    );
                    bind(&mut template_ctx, val, placeholder());
                }
                ForBinding::Map(k, v) => {
                    bind(
                        &mut template_ctx,
                        k,
                        Value::String(DEFERRED_UPSTREAM_KEY_PLACEHOLDER.to_string()),
                    );
                    bind(&mut template_ctx, v, placeholder());
                }
            }

            let address = format!("{}[?]", binding_name);
            if let Ok(ForBodyResult::Resource(resource)) =
                parse_for_body(body_pair, &template_ctx, &address)
            {
                let attrs: Vec<(String, Value)> = resource
                    .attributes
                    .iter()
                    .filter(|(k, _)| !k.starts_with('_'))
                    .map(|(k, expr)| (k.clone(), expr.0.clone()))
                    .collect();
                ctx.deferred_for_expressions.push(DeferredForExpression {
                    file: None,
                    line: for_line,
                    header,
                    resource_type: if resource.id.provider.is_empty() {
                        resource.id.resource_type.clone()
                    } else {
                        format!("{}.{}", resource.id.provider, resource.id.resource_type)
                    },
                    attributes: attrs,
                    binding_name: binding_name.to_string(),
                    iterable_binding: path.binding().to_string(),
                    iterable_attr: path.attribute().to_string(),
                    binding: binding.clone(),
                    template_resource: *resource,
                });
            }
            // Return empty — the for body produces zero concrete resources
        }
        _ => {
            // Special case: the parser collapses bare unresolved identifiers
            // (e.g. `for _ in org { ... }`) into `Value::String("org")` — the
            // same slot a quoted literal uses. Reporting those as
            // `iterable is string "org"` is misleading: the user wrote an
            // identifier, not a literal, and the likely fault is a typo for
            // a known binding. Record them as deferred for-expressions so
            // `check_identifier_scope` (which runs on the merged
            // directory-wide ParsedFile, so cross-file upstream_state /
            // module bindings are visible) can emit a proper
            // UndefinedIdentifier with the did-you-mean machinery from
            // #2038 / #2100. See #2101 / #2138.
            if let Value::String(s) = &iterable
                && is_bare_identifier(s)
            {
                let header = match &binding {
                    ForBinding::Simple(var) => format!("for {} in {}", var, s),
                    ForBinding::Indexed(idx, val) => format!("for ({}, {}) in {}", idx, val, s),
                    ForBinding::Map(k, v) => format!("for {}, {} in {}", k, v, s),
                };
                let mut template_ctx = ctx.clone();
                let placeholder = || Value::String(DEFERRED_UPSTREAM_PLACEHOLDER.to_string());
                let bind = |c: &mut ParseContext, name: &str, v: Value| {
                    if name != "_" {
                        c.set_variable(name.to_string(), v);
                    }
                };
                match &binding {
                    ForBinding::Simple(var) => {
                        bind(&mut template_ctx, var, placeholder());
                    }
                    ForBinding::Indexed(idx, val) => {
                        bind(
                            &mut template_ctx,
                            idx,
                            Value::String(DEFERRED_UPSTREAM_INDEX_PLACEHOLDER.to_string()),
                        );
                        bind(&mut template_ctx, val, placeholder());
                    }
                    ForBinding::Map(k, v) => {
                        bind(
                            &mut template_ctx,
                            k,
                            Value::String(DEFERRED_UPSTREAM_KEY_PLACEHOLDER.to_string()),
                        );
                        bind(&mut template_ctx, v, placeholder());
                    }
                }
                let address = format!("{}[?]", binding_name);
                if let Ok(ForBodyResult::Resource(resource)) =
                    parse_for_body(body_pair, &template_ctx, &address)
                {
                    let attrs: Vec<(String, Value)> = resource
                        .attributes
                        .iter()
                        .filter(|(k, _)| !k.starts_with('_'))
                        .map(|(k, expr)| (k.clone(), expr.0.clone()))
                        .collect();
                    ctx.deferred_for_expressions.push(DeferredForExpression {
                        file: None,
                        line: for_line,
                        header,
                        resource_type: if resource.id.provider.is_empty() {
                            resource.id.resource_type.clone()
                        } else {
                            format!("{}.{}", resource.id.provider, resource.id.resource_type)
                        },
                        attributes: attrs,
                        binding_name: binding_name.to_string(),
                        iterable_binding: s.clone(),
                        iterable_attr: String::new(),
                        binding: binding.clone(),
                        template_resource: *resource,
                    });
                }
                return Ok((resources, module_calls));
            }
            let iterable_type = match &iterable {
                Value::String(s) => {
                    format!("string \"{}\"", if s.len() > 50 { &s[..50] } else { s })
                }
                Value::Int(i) => format!("int {}", i),
                Value::Float(f) => format!("float {}", f),
                Value::Bool(b) => format!("bool {}", b),
                Value::ResourceRef { path } => {
                    format!("unresolved reference {}", path.to_dot_string())
                }
                Value::List(_) => "list".to_string(),
                Value::Map(_) => "map".to_string(),
                other => format!("{:?}", other),
            };
            let binding_type = match &binding {
                ForBinding::Simple(var) => format!("`for {} in ...`", var),
                ForBinding::Indexed(idx, val) => format!("`for {}, {} in ...`", idx, val),
                ForBinding::Map(k, v) => format!("`for {}, {} in ...`", k, v),
            };
            let expected = match &binding {
                ForBinding::Simple(_) | ForBinding::Indexed(_, _) => "list",
                ForBinding::Map(_, _) => "map",
            };
            return Err(ParseError::InvalidExpression {
                line: for_line,
                message: format!(
                    "{} — iterable is {} (expected {})",
                    binding_type, iterable_type, expected
                ),
            });
        }
    }

    Ok((resources, module_calls))
}

/// Parse a for binding pattern.
///
/// Each position accepts either an `identifier` or a `discard_pattern`
/// (`_`). The text of the matched pair — either the identifier name or
/// the literal `_` — is stored as the binding name. A `_` marker is not
/// added to the parse-time scope and is exempt from the unused-binding
/// warning; downstream code checks for `name == "_"` to enforce that.
fn parse_for_binding(pair: pest::iterators::Pair<Rule>) -> Result<ForBinding, ParseError> {
    let inner = first_inner(pair, "binding pattern", "for binding")?;
    match inner.as_rule() {
        Rule::for_simple_binding => {
            let name = first_inner(inner, "identifier", "simple binding")?
                .as_str()
                .to_string();
            Ok(ForBinding::Simple(name))
        }
        Rule::for_indexed_binding => {
            let mut parts = inner.into_inner();
            let idx = next_pair(&mut parts, "index variable", "indexed binding")?
                .as_str()
                .to_string();
            let val = next_pair(&mut parts, "value variable", "indexed binding")?
                .as_str()
                .to_string();
            Ok(ForBinding::Indexed(idx, val))
        }
        Rule::for_map_binding => {
            let mut parts = inner.into_inner();
            let key = next_pair(&mut parts, "key variable", "map binding")?
                .as_str()
                .to_string();
            let val = next_pair(&mut parts, "value variable", "map binding")?
                .as_str()
                .to_string();
            Ok(ForBinding::Map(key, val))
        }
        _ => Err(ParseError::InternalError {
            expected: "for binding pattern".to_string(),
            context: "for expression".to_string(),
        }),
    }
}

/// Parse the iterable part of a for expression
///
/// When the iterable is a function call with all statically-known arguments,
/// the function is eagerly evaluated at parse time. If any argument depends on
/// a runtime value (e.g. ResourceRef), a clear error is returned.
fn parse_for_iterable(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    // for_iterable contains function_call | list | variable_ref | "(" expression ")"
    let inner = first_inner(pair, "iterable expression", "for iterable")?;
    let eval = parse_primary_eval(inner, ctx)?;
    let value = eval
        .into_value()
        .map_err(|leak| ParseError::InvalidExpression {
            line: 0,
            message: format!(
                "for iterable evaluates to a closure '{}' (still needs {} arg(s)); \
             closures cannot be iterated",
                leak.name, leak.remaining_arity
            ),
        })?;
    evaluate_static_value(value, ctx.config)
}

/// Check whether a Value is fully static (no runtime dependencies).
fn is_static_value(value: &Value) -> bool {
    match value {
        Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) => true,
        Value::List(items) => items.iter().all(is_static_value),
        Value::Map(map) => map.values().all(is_static_value),
        Value::FunctionCall { args, .. } => args.iter().all(is_static_value),
        Value::ResourceRef { .. } | Value::Interpolation(_) => false,
        Value::Secret(inner) => is_static_value(inner),
    }
}

/// `is_static_value` for the evaluator-internal `EvalValue` type.
/// A closure's static-ness is decided by whether all of its captured
/// args are themselves static. The pipe/compose paths use this when
/// they need to decide whether to eagerly apply a partial application.
fn is_static_eval(value: &EvalValue) -> bool {
    match value {
        EvalValue::User(v) => is_static_value(v),
        EvalValue::Closure { captured_args, .. } => captured_args.iter().all(is_static_eval),
    }
}

/// If `value` is a FunctionCall with all static arguments, eagerly evaluate it.
/// Nested FunctionCalls in arguments are evaluated recursively first.
fn evaluate_static_value(value: Value, config: &ProviderContext) -> Result<Value, ParseError> {
    match value {
        Value::FunctionCall { ref name, ref args } => {
            if !is_static_value(&value) {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: format!(
                        "for iterable function call '{name}' depends on a runtime value; \
                         all arguments must be statically known at parse time"
                    ),
                });
            }
            // Recursively evaluate any nested FunctionCall arguments
            let evaluated_args: Result<Vec<Value>, ParseError> = args
                .iter()
                .cloned()
                .map(|v| evaluate_static_value(v, config))
                .collect();
            let evaluated_args = evaluated_args?;
            let eval_args: Vec<EvalValue> = evaluated_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            let result = crate::builtins::evaluate_builtin_with_config(name, &eval_args, config)
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("for iterable function call '{name}' failed: {e}"),
                })?;
            result
                .into_value()
                .map_err(|leak| ParseError::InvalidExpression {
                    line: 0,
                    message: format!(
                        "for iterable function call '{name}' returned a closure '{}' \
                     (still needs {} arg(s)); finish the partial application",
                        leak.name, leak.remaining_arity
                    ),
                })
        }
        other => Ok(other),
    }
}

/// Parse the body of a for expression and produce a single resource or module call
fn parse_for_body(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    address: &str,
) -> Result<ForBodyResult, ParseError> {
    let mut local_ctx = ctx.clone();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::for_local_binding => {
                let mut binding_inner = inner.into_inner();
                let name = next_pair(&mut binding_inner, "binding name", "for local binding")?
                    .as_str()
                    .to_string();
                let value = parse_expression(
                    next_pair(&mut binding_inner, "binding value", "for local binding")?,
                    &local_ctx,
                )?;
                local_ctx.set_variable(name, value);
            }
            Rule::resource_expr => {
                let resource = parse_resource_expr(inner, &local_ctx, address)?;
                return Ok(ForBodyResult::Resource(Box::new(resource)));
            }
            Rule::read_resource_expr => {
                let resource = parse_read_resource_expr(inner, &local_ctx, address)?;
                return Ok(ForBodyResult::Resource(Box::new(resource)));
            }
            Rule::module_call => {
                let mut call = parse_module_call(inner, &local_ctx)?;
                call.binding_name = Some(address.to_string());
                return Ok(ForBodyResult::ModuleCall(call));
            }
            _ => {}
        }
    }

    Err(ParseError::InternalError {
        expected: "resource expression or module call".to_string(),
        context: "for body".to_string(),
    })
}

/// Result of parsing an if expression body: a resource, a module call, or a value
enum IfBodyResult {
    Resource(Box<Resource>),
    ModuleCall(ModuleCall),
    Value(Value),
}

/// Parse an if expression and conditionally include resources/module calls/values.
///
/// `if condition { body }` includes the body when condition is true.
/// `if condition { body } else { body }` selects one branch.
///
/// The condition must evaluate to a static Bool value at parse time.
fn parse_if_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &mut ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let mut inner = pair.into_inner();

    // Parse the condition expression
    let condition_pair = next_pair(&mut inner, "condition", "if expression")?;
    let condition_value = parse_expression(condition_pair, ctx)?;

    // Ensure the condition is statically evaluable
    if !is_static_value(&condition_value) {
        return Err(ParseError::InvalidExpression {
            line: 0,
            message: "if condition depends on a runtime value; \
                      condition must be statically known at parse time"
                .to_string(),
        });
    }

    let condition_value = evaluate_static_value(condition_value, ctx.config)?;

    // Condition must be a Bool
    let condition = match &condition_value {
        Value::Bool(b) => *b,
        other => {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: format!("if condition must be a Bool value, got: {:?}", other),
            });
        }
    };

    // Parse the if body
    let if_body_pair = next_pair(&mut inner, "if body", "if expression")?;

    // Check for else clause
    let else_body_pair = inner.next();

    if condition {
        // Use the if branch
        parse_if_body_to_rhs(if_body_pair, ctx, binding_name)
    } else if let Some(else_pair) = else_body_pair {
        // Use the else branch
        let else_body = first_inner(else_pair, "else body", "else clause")?;
        parse_if_body_to_rhs(else_body, ctx, binding_name)
    } else {
        // No else clause and condition is false: produce nothing
        let ref_value = Value::String(format!("${{if:{}}}", binding_name));
        Ok((EvalValue::from_value(ref_value), vec![], vec![], None))
    }
}

/// Parse an if/else body and convert the result to a LetBindingRhs
fn parse_if_body_to_rhs(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<LetBindingRhs, ParseError> {
    let result = parse_if_body(pair, ctx, binding_name)?;
    match result {
        IfBodyResult::Resource(r) => {
            let ref_value = Value::String(format!("${{{}}}", binding_name));
            Ok((EvalValue::from_value(ref_value), vec![*r], vec![], None))
        }
        IfBodyResult::ModuleCall(c) => {
            let value = Value::String(format!("${{module:{}}}", c.module_name));
            Ok((EvalValue::from_value(value), vec![], vec![c], None))
        }
        IfBodyResult::Value(v) => Ok((EvalValue::from_value(v), vec![], vec![], None)),
    }
}

/// Parse the body of an if expression and produce a resource, module call, or value
fn parse_if_body(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<IfBodyResult, ParseError> {
    let mut local_ctx = ctx.clone();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::if_local_binding => {
                let mut binding_inner = inner.into_inner();
                let name = next_pair(&mut binding_inner, "binding name", "if local binding")?
                    .as_str()
                    .to_string();
                let value = parse_expression(
                    next_pair(&mut binding_inner, "binding value", "if local binding")?,
                    &local_ctx,
                )?;
                local_ctx.set_variable(name, value);
            }
            Rule::resource_expr => {
                let resource = parse_resource_expr(inner, &local_ctx, binding_name)?;
                return Ok(IfBodyResult::Resource(Box::new(resource)));
            }
            Rule::read_resource_expr => {
                let resource = parse_read_resource_expr(inner, &local_ctx, binding_name)?;
                return Ok(IfBodyResult::Resource(Box::new(resource)));
            }
            Rule::module_call => {
                let mut call = parse_module_call(inner, &local_ctx)?;
                call.binding_name = Some(binding_name.to_string());
                return Ok(IfBodyResult::ModuleCall(call));
            }
            Rule::expression => {
                let value = parse_expression(inner, &local_ctx)?;
                return Ok(IfBodyResult::Value(value));
            }
            _ => {}
        }
    }

    Err(ParseError::InternalError {
        expected: "resource expression, module call, or value expression".to_string(),
        context: "if body".to_string(),
    })
}

/// Parse an if/else expression in value position (attribute values, not let bindings).
///
/// Unlike `parse_if_expr()` which returns `LetBindingRhs` (resources, module calls, or values),
/// this function only returns `Value`. The condition must be a static Bool.
/// An else clause is required when the condition is false (a value must always be determined).
fn parse_if_value_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let mut inner = pair.into_inner();

    // Parse the condition expression
    let condition_pair = next_pair(&mut inner, "condition", "if value expression")?;
    let condition_value = parse_expression(condition_pair, ctx)?;

    // Ensure the condition is statically evaluable
    if !is_static_value(&condition_value) {
        return Err(ParseError::InvalidExpression {
            line: 0,
            message: "if condition depends on a runtime value; \
                      condition must be statically known at parse time"
                .to_string(),
        });
    }

    let condition_value = evaluate_static_value(condition_value, ctx.config)?;

    // Condition must be a Bool
    let condition = match &condition_value {
        Value::Bool(b) => *b,
        other => {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: format!("if condition must be a Bool value, got: {:?}", other),
            });
        }
    };

    // Parse the if body
    let if_body_pair = next_pair(&mut inner, "if body", "if value expression")?;

    // Check for else clause
    let else_body_pair = inner.next();

    if condition {
        parse_if_body_value(if_body_pair, ctx)
    } else if let Some(else_pair) = else_body_pair {
        let else_body = first_inner(else_pair, "else body", "else clause")?;
        parse_if_body_value(else_body, ctx)
    } else {
        Err(ParseError::InvalidExpression {
            line: 0,
            message: "if expression in value position requires an else clause \
                      when condition is false"
                .to_string(),
        })
    }
}

/// Parse the body of an if expression in value position and return only the value.
fn parse_if_body_value(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let mut local_ctx = ctx.clone();

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::if_local_binding => {
                let mut binding_inner = inner.into_inner();
                let name = next_pair(&mut binding_inner, "binding name", "if local binding")?
                    .as_str()
                    .to_string();
                let value = parse_expression(
                    next_pair(&mut binding_inner, "binding value", "if local binding")?,
                    &local_ctx,
                )?;
                local_ctx.set_variable(name, value);
            }
            Rule::expression => {
                return parse_expression(inner, &local_ctx);
            }
            Rule::resource_expr | Rule::read_resource_expr | Rule::module_call => {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: "resource expressions and module calls cannot be used \
                              in if value expressions; use a let binding instead"
                        .to_string(),
                });
            }
            _ => {}
        }
    }

    Err(ParseError::InternalError {
        expected: "value expression".to_string(),
        context: "if value expression body".to_string(),
    })
}

fn parse_provider_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<ProviderConfig, ParseError> {
    let mut inner = pair.into_inner();
    let name = next_pair(&mut inner, "provider name", "provider block")?
        .as_str()
        .to_string();

    let mut attributes: IndexMap<String, Value> = IndexMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let mut attr_inner = attr_pair.into_inner();
            let key = next_pair(&mut attr_inner, "attribute name", "provider block")?
                .as_str()
                .to_string();
            let value = parse_expression(
                next_pair(&mut attr_inner, "attribute value", "provider block")?,
                ctx,
            )?;
            attributes.insert(key, value);
        }
    }

    // `shift_remove` keeps the surviving attributes in source order.
    // Extract default_tags from attributes if present
    let default_tags = if let Some(Value::Map(tags)) = attributes.shift_remove("default_tags") {
        tags
    } else {
        IndexMap::new()
    };

    // Extract source from attributes if present
    let source = if let Some(Value::String(s)) = attributes.shift_remove("source") {
        Some(s)
    } else {
        None
    };

    // Extract version from attributes if present
    let version = if let Some(Value::String(v)) = attributes.shift_remove("version") {
        Some(VersionConstraint::parse(&v).map_err(|e| {
            pest::error::Error::new_from_pos(
                pest::error::ErrorVariant::CustomError { message: e },
                pest::Position::from_start(""),
            )
        })?)
    } else {
        None
    };

    // Extract revision from attributes if present
    let revision = if let Some(Value::String(r)) = attributes.shift_remove("revision") {
        Some(r)
    } else {
        None
    };

    // Validate that version and revision are mutually exclusive
    if version.is_some() && revision.is_some() {
        return Err(ParseError::Syntax(pest::error::Error::new_from_pos(
            pest::error::ErrorVariant::CustomError {
                message: format!(
                    "Provider '{}': 'version' and 'revision' are mutually exclusive",
                    name
                ),
            },
            pest::Position::from_start(""),
        )));
    }

    Ok(ProviderConfig {
        name,
        attributes,
        default_tags,
        source,
        version,
        revision,
    })
}

/// Split a namespaced identifier (e.g., "awscc.ec2.Vpc") into (provider, resource_type)
fn split_namespaced_id(namespaced: &str) -> (String, String) {
    let parts: Vec<&str> = namespaced.split('.').collect();
    if parts.len() >= 2 {
        (parts[0].to_string(), parts[1..].join("."))
    } else {
        (String::new(), namespaced.to_string())
    }
}

/// Parse a resource address: `provider.service.type "name"`
fn parse_resource_address(pair: pest::iterators::Pair<Rule>) -> Result<ResourceId, ParseError> {
    let mut inner = pair.into_inner();
    let namespaced = next_pair(&mut inner, "namespaced id", "resource address")?
        .as_str()
        .to_string();
    let name_pair = next_pair(&mut inner, "resource name", "resource address")?;
    // The name is a string literal - extract value from quotes
    let name = parse_string_literal(name_pair)?;

    // Split namespaced id into provider and resource_type
    let (provider, resource_type) = split_namespaced_id(&namespaced);

    Ok(ResourceId::with_provider(provider, resource_type, name))
}

/// Parse a string token into its literal value (without quotes).
/// Only handles plain strings (no interpolation).
fn parse_string_literal(pair: pest::iterators::Pair<Rule>) -> Result<String, ParseError> {
    // string = single_quoted_string | double_quoted_string
    let inner_pair = pair.into_inner().next().unwrap();

    if inner_pair.as_rule() == Rule::single_quoted_string {
        return Ok(inner_pair
            .into_inner()
            .next()
            .map(|p| unescape_single_quoted(p.as_str()))
            .unwrap_or_default());
    }

    // Double-quoted string
    let mut result = String::new();
    for part in inner_pair.into_inner() {
        if part.as_rule() == Rule::string_part {
            for inner in part.into_inner() {
                if inner.as_rule() == Rule::string_literal {
                    result.push_str(inner.as_str());
                }
            }
        }
    }
    Ok(result)
}

/// Parse an import state block
fn parse_import_state_block(pair: pest::iterators::Pair<Rule>) -> Result<StateBlock, ParseError> {
    let mut to: Option<ResourceId> = None;
    let mut id: Option<String> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::import_state_attr {
            let inner = first_inner(attr, "import attribute", "import block")?;
            match inner.as_rule() {
                Rule::import_to_attr => {
                    let addr = first_inner(inner, "resource address", "import to")?;
                    to = Some(parse_resource_address(addr)?);
                }
                Rule::import_id_attr => {
                    let str_pair = first_inner(inner, "string", "import id")?;
                    id = Some(parse_string_literal(str_pair)?);
                }
                _ => {}
            }
        }
    }

    let to = to.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "import block requires 'to' attribute".to_string(),
    })?;
    let id = id.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "import block requires 'id' attribute".to_string(),
    })?;

    Ok(StateBlock::Import { to, id })
}

/// Parse a removed block
fn parse_removed_block(pair: pest::iterators::Pair<Rule>) -> Result<StateBlock, ParseError> {
    let mut from: Option<ResourceId> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::removed_attr {
            let addr = first_inner(attr, "resource address", "removed from")?;
            from = Some(parse_resource_address(addr)?);
        }
    }

    let from = from.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "removed block requires 'from' attribute".to_string(),
    })?;

    Ok(StateBlock::Removed { from })
}

/// Parse a moved block
fn parse_moved_block(pair: pest::iterators::Pair<Rule>) -> Result<StateBlock, ParseError> {
    let mut from: Option<ResourceId> = None;
    let mut to: Option<ResourceId> = None;

    for attr in pair.into_inner() {
        if attr.as_rule() == Rule::moved_attr {
            let inner = first_inner(attr, "moved attribute", "moved block")?;
            match inner.as_rule() {
                Rule::moved_from_attr => {
                    let addr = first_inner(inner, "resource address", "moved from")?;
                    from = Some(parse_resource_address(addr)?);
                }
                Rule::moved_to_attr => {
                    let addr = first_inner(inner, "resource address", "moved to")?;
                    to = Some(parse_resource_address(addr)?);
                }
                _ => {}
            }
        }
    }

    let from = from.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "moved block requires 'from' attribute".to_string(),
    })?;
    let to = to.ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "moved block requires 'to' attribute".to_string(),
    })?;

    Ok(StateBlock::Moved { from, to })
}

/// Parse a user-defined function definition
fn parse_fn_def(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    warnings: &mut Vec<ParseWarning>,
) -> Result<UserFunction, ParseError> {
    let mut inner = pair.into_inner();
    let name = next_pair(&mut inner, "function name", "fn_def")?
        .as_str()
        .to_string();

    // Parse parameters (optional)
    let mut params = Vec::new();
    let next = next_pair(&mut inner, "fn_params or fn_body", "fn_def")?;
    let next_token = if next.as_rule() == Rule::fn_params {
        // Parse parameter list
        for param_pair in next.into_inner() {
            if param_pair.as_rule() == Rule::fn_param {
                let mut param_inner = param_pair.into_inner();
                let param_name = next_pair(&mut param_inner, "parameter name", "fn_param")?
                    .as_str()
                    .to_string();
                // Parse optional type annotation (: type_expr)
                let mut param_type = None;
                let mut default = None;
                for remaining in param_inner {
                    match remaining.as_rule() {
                        Rule::type_expr => {
                            param_type = Some(parse_type_expr(remaining, ctx.config, warnings)?);
                        }
                        _ => {
                            // This is the default expression
                            let default_ctx = ParseContext::new(ctx.config);
                            default = Some(parse_expression(remaining, &default_ctx)?);
                        }
                    }
                }
                // Validate: required params must come before optional params
                if default.is_none() && params.iter().any(|p: &FnParam| p.default.is_some()) {
                    return Err(ParseError::UserFunctionError(format!(
                        "in function '{name}': required parameter '{param_name}' cannot follow optional parameter"
                    )));
                }
                params.push(FnParam {
                    name: param_name,
                    param_type,
                    default,
                });
            }
        }
        next_pair(&mut inner, "type_expr or fn_body", "fn_def")?
    } else {
        next
    };

    // Parse optional return type annotation (: type_expr)
    let (return_type, body_pair) = if next_token.as_rule() == Rule::type_expr {
        let rt = parse_type_expr(next_token, ctx.config, warnings)?;
        let bp = next_pair(&mut inner, "fn_body", "fn_def")?;
        (Some(rt), bp)
    } else {
        (None, next_token)
    };

    // Parse body: fn_local_let* ~ (resource_expr | read_resource_expr | expression)
    let mut local_lets = Vec::new();
    let mut body: Option<UserFunctionBody> = None;

    // Create a context where parameters are registered as variables
    // so that param references in the body are resolved as variable refs
    let mut body_ctx = ParseContext::new(ctx.config);
    for p in &params {
        body_ctx.set_variable(
            p.name.clone(),
            Value::String(format!("__fn_param_{}", p.name)),
        );
    }

    for body_inner in body_pair.into_inner() {
        match body_inner.as_rule() {
            Rule::fn_local_let => {
                let mut let_inner = body_inner.into_inner();
                let let_name = next_pair(&mut let_inner, "let name", "fn_local_let")?
                    .as_str()
                    .to_string();
                let let_expr = parse_expression(
                    next_pair(&mut let_inner, "let expression", "fn_local_let")?,
                    &body_ctx,
                )?;
                body_ctx.set_variable(
                    let_name.clone(),
                    Value::String(format!("__fn_local_{let_name}")),
                );
                local_lets.push((let_name, let_expr));
            }
            _ => {
                // This should be the expression (the body)
                body = Some(UserFunctionBody(parse_expression(body_inner, &body_ctx)?));
            }
        }
    }

    let body = body.ok_or_else(|| ParseError::InternalError {
        expected: "body expression".to_string(),
        context: "fn_def".to_string(),
    })?;

    Ok(UserFunction {
        name,
        params,
        return_type,
        local_lets,
        body,
    })
}

/// Prepare a user-defined function call: validate args, build substitutions, and return
/// the child context with all parameters and local lets resolved.
fn prepare_user_function_call<'cfg>(
    func: &UserFunction,
    args: &[Value],
    ctx: &ParseContext<'cfg>,
) -> Result<(ParseContext<'cfg>, HashMap<String, Value>), ParseError> {
    let fn_name = &func.name;

    // Check recursion
    if ctx.evaluating_functions.contains(fn_name) {
        return Err(ParseError::RecursiveFunction(fn_name.clone()));
    }

    // Validate argument count
    let required_count = func.params.iter().filter(|p| p.default.is_none()).count();
    let max_count = func.params.len();
    if args.len() < required_count {
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}' expects at least {required_count} argument(s), got {}",
            args.len()
        )));
    }
    if args.len() > max_count {
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}' expects at most {max_count} argument(s), got {}",
            args.len()
        )));
    }

    // Build substitution map: param_name -> value, and type-check annotated params
    let mut substitutions: HashMap<String, Value> = HashMap::new();
    for (i, param) in func.params.iter().enumerate() {
        let value = if i < args.len() {
            args[i].clone()
        } else {
            param.default.clone().unwrap()
        };
        // Type-check if the parameter has a type annotation
        if let Some(ref type_expr) = param.param_type {
            check_fn_arg_type(fn_name, &param.name, type_expr, &value, ctx)?;
        }
        substitutions.insert(param.name.clone(), value);
    }

    // Create a child context with recursion tracking
    let mut child_ctx = ctx.clone();
    child_ctx.evaluating_functions.push(fn_name.clone());

    // Evaluate local lets, substituting and resolving each one
    for (let_name, let_expr) in &func.local_lets {
        let substituted = substitute_fn_params(let_expr, &substitutions);
        let evaluated = try_evaluate_fn_value(substituted, &child_ctx)?;
        child_ctx.set_variable(let_name.clone(), evaluated.clone());
        substitutions.insert(let_name.clone(), evaluated);
    }

    Ok((child_ctx, substitutions))
}

/// Validate a value against a custom type (ipv4_cidr, ipv4_address, etc.).
/// Returns Ok(()) if the value passes validation or cannot be validated statically
/// (e.g., ResourceRef, FunctionCall, Interpolation are deferred).
///
/// Checks built-in validators first, then falls back to custom validators
/// registered in the [`ProviderContext`].
pub fn validate_custom_type(
    type_name: &str,
    value: &Value,
    config: &ProviderContext,
) -> Result<(), String> {
    match (type_name, value) {
        ("ipv4_cidr", Value::String(s)) => validate_ipv4_cidr(s),
        ("ipv4_address", Value::String(s)) => validate_ipv4_address(s),
        ("ipv6_cidr", Value::String(s)) => validate_ipv6_cidr(s),
        ("ipv6_address", Value::String(s)) => validate_ipv6_address(s),
        (_, Value::ResourceRef { .. }) => Ok(()), // will be resolved later
        (_, Value::FunctionCall { .. }) => Ok(()), // will be resolved later
        (_, Value::Interpolation(_)) => Ok(()),   // will be resolved later
        (name, Value::String(s)) => {
            // Check custom validators from config (schema-extracted)
            if let Some(validator) = config.validators.get(name) {
                validator(s)?;
            }
            // Fall back to factory-based validator (e.g., WASM providers)
            if let Some(ref factory_validator) = config.custom_type_validator {
                factory_validator(name, s)
            } else {
                Ok(())
            }
        }
        (_, value) => Err(format!(
            "expected {}, got {}",
            type_name,
            value_type_name(value)
        )),
    }
}

/// Check that a function argument matches the declared parameter type.
fn check_fn_arg_type(
    fn_name: &str,
    param_name: &str,
    type_expr: &TypeExpr,
    value: &Value,
    ctx: &ParseContext,
) -> Result<(), ParseError> {
    let type_matches = match type_expr {
        TypeExpr::String => matches!(
            value,
            Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
        ),
        TypeExpr::Int => matches!(value, Value::Int(_)),
        TypeExpr::Float => matches!(value, Value::Float(_)),
        TypeExpr::Bool => matches!(value, Value::Bool(_)),
        TypeExpr::List(_) => matches!(value, Value::List(_)),
        TypeExpr::Map(_) => matches!(value, Value::Map(_)),
        // Simple types (cidr, ipv4_address, arn, etc.) are string subtypes at runtime
        TypeExpr::Simple(name) => {
            if !matches!(
                value,
                Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
            ) {
                false
            } else {
                // Validate the actual value against the custom type
                if let Err(e) = validate_custom_type(name, value, ctx.config) {
                    return Err(ParseError::UserFunctionError(format!(
                        "function '{fn_name}': parameter '{param_name}' type '{name}' validation failed: {e}"
                    )));
                }
                true
            }
        }
        // Resource type refs: check that the argument is a binding of the correct resource type
        TypeExpr::Ref(expected_path) => {
            // The argument is passed as a ResourceRef-like string "${binding_name}"
            // or as a direct ResourceRef. Check if it corresponds to a resource binding
            // of the expected type.
            if let Value::String(s) = value
                && let Some(ref_name) = s.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
                && let Some(resource) = ctx.resource_bindings.get(ref_name)
            {
                let actual_provider = &resource.id.provider;
                let actual_type = &resource.id.resource_type;
                if actual_provider != &expected_path.provider
                    || actual_type != &expected_path.resource_type
                {
                    return Err(ParseError::UserFunctionError(format!(
                        "function '{fn_name}': parameter '{param_name}' expects resource type '{expected_path}', got {actual_provider}.{actual_type}"
                    )));
                }
            }
            // If not found in bindings, skip validation (forward ref or dynamic)
            true
        }
        // Schema types (awscc.ec2.VpcId, etc.) are string subtypes with provider validators
        TypeExpr::SchemaType { type_name, .. } => {
            if !matches!(
                value,
                Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
            ) {
                false
            } else {
                // Convert PascalCase type_name to snake_case for validator lookup
                let validator_key = pascal_to_snake(type_name);
                if let Err(e) = validate_custom_type(&validator_key, value, ctx.config) {
                    return Err(ParseError::UserFunctionError(format!(
                        "function '{fn_name}': parameter '{param_name}' type '{type_expr}' validation failed: {e}"
                    )));
                }
                true
            }
        }
        TypeExpr::Struct { .. } => matches!(value, Value::Map(_)),
    };
    if !type_matches {
        let actual_type = value_type_name(value);
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}': parameter '{param_name}' expects type '{type_expr}', got {actual_type}"
        )));
    }
    Ok(())
}

/// Check that a function's return value matches the declared return type.
fn check_fn_return_type(
    fn_name: &str,
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    let type_matches = match type_expr {
        TypeExpr::String => matches!(
            value,
            Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
        ),
        TypeExpr::Int => matches!(value, Value::Int(_)),
        TypeExpr::Float => matches!(value, Value::Float(_)),
        TypeExpr::Bool => matches!(value, Value::Bool(_)),
        TypeExpr::List(_) => matches!(value, Value::List(_)),
        TypeExpr::Map(_) => matches!(value, Value::Map(_)),
        // Simple types (cidr, ipv4_address, arn, etc.) — validate the value
        TypeExpr::Simple(name) => {
            if !matches!(
                value,
                Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
            ) {
                false
            } else {
                if let Err(e) = validate_custom_type(name, value, config) {
                    return Err(ParseError::UserFunctionError(format!(
                        "function '{fn_name}': return type '{name}' validation failed: {e}"
                    )));
                }
                true
            }
        }
        // Resource type refs: not applicable for value functions
        TypeExpr::Ref(_) => true,
        // Schema types: validate returned value against the provider validator
        TypeExpr::SchemaType { type_name, .. } => {
            if !matches!(
                value,
                Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
            ) {
                false
            } else {
                let validator_key = pascal_to_snake(type_name);
                if let Err(e) = validate_custom_type(&validator_key, value, config) {
                    return Err(ParseError::UserFunctionError(format!(
                        "function '{fn_name}': return type '{type_name}' validation failed: {e}"
                    )));
                }
                true
            }
        }
        TypeExpr::Struct { .. } => matches!(value, Value::Map(_)),
    };
    if !type_matches {
        let actual_type = value_type_name(value);
        return Err(ParseError::UserFunctionError(format!(
            "function '{fn_name}': return type '{type_expr}' does not match actual return value of type {actual_type}"
        )));
    }
    Ok(())
}

/// Convert PascalCase to snake_case (e.g., "VpcId" → "vpc_id", "SubnetId" → "subnet_id")
pub fn pascal_to_snake(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_lowercase().next().unwrap());
        } else {
            result.push(c);
        }
    }
    result
}

/// Convert snake_case to PascalCase (e.g., "vpc_id" → "VpcId", "aws_account_id" → "AwsAccountId").
///
/// Acronyms are treated as regular words (`iam_policy_arn` → `IamPolicyArn`,
/// `ipv4_cidr` → `Ipv4Cidr`) so that the result matches `semantic_name` values
/// already produced by `pascal_to_snake` and is a round-trip inverse for them.
pub fn snake_to_pascal(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = true;
    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

/// Return a human-readable type name for a Value
pub(crate) fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::List(_) => "list",
        Value::Map(_) => "map",
        Value::ResourceRef { .. } => "resource reference",
        Value::Interpolation(_) => "string",
        Value::FunctionCall { .. } => "function call",
        Value::Secret(_) => "secret",
    }
}

/// Return a human-readable type name for an `EvalValue`. Closures only
/// exist on the evaluator-internal type, so this is the version used by
/// pipe/compose error messages where a closure can legitimately show up.
pub(crate) fn eval_type_name(value: &EvalValue) -> &'static str {
    match value {
        EvalValue::User(v) => value_type_name(v),
        EvalValue::Closure { .. } => "closure",
    }
}

/// Evaluate a user-defined function call by substituting arguments into the body
fn evaluate_user_function(
    func: &UserFunction,
    args: &[Value],
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let (child_ctx, substitutions) = prepare_user_function_call(func, args, ctx)?;

    let UserFunctionBody(body) = &func.body;
    let substituted_body = substitute_fn_params(body, &substitutions);
    let result = try_evaluate_fn_value(substituted_body, &child_ctx)?;
    // Check return type if annotated
    if let Some(ref return_type) = func.return_type {
        check_fn_return_type(&func.name, return_type, &result, child_ctx.config)?;
    }
    Ok(result)
}

/// Recursively substitute function parameter placeholders with actual values
fn substitute_fn_params(value: &Value, substitutions: &HashMap<String, Value>) -> Value {
    match value {
        Value::String(s) => {
            // Check if this is a parameter placeholder
            if let Some(param_name) = s.strip_prefix("__fn_param_")
                && let Some(sub) = substitutions.get(param_name)
            {
                return sub.clone();
            }
            if let Some(local_name) = s.strip_prefix("__fn_local_")
                && let Some(sub) = substitutions.get(local_name)
            {
                return sub.clone();
            }
            Value::String(s.clone())
        }
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|v| substitute_fn_params(v, substitutions))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_fn_params(v, substitutions)))
                .collect(),
        ),
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| substitute_fn_params(a, substitutions))
                .collect(),
        },
        Value::Interpolation(parts) => Value::Interpolation(
            parts
                .iter()
                .map(|p| match p {
                    crate::resource::InterpolationPart::Expr(v) => {
                        crate::resource::InterpolationPart::Expr(substitute_fn_params(
                            v,
                            substitutions,
                        ))
                    }
                    other => other.clone(),
                })
                .collect(),
        ),
        Value::Secret(inner) => Value::Secret(Box::new(substitute_fn_params(inner, substitutions))),
        other => other.clone(),
    }
}

/// Try to evaluate a value (resolve function calls including user-defined ones)
fn try_evaluate_fn_value(value: Value, ctx: &ParseContext) -> Result<Value, ParseError> {
    match value {
        Value::FunctionCall { ref name, ref args } => {
            // First, recursively evaluate arguments
            let evaluated_args: Result<Vec<Value>, ParseError> = args
                .iter()
                .map(|a| try_evaluate_fn_value(a.clone(), ctx))
                .collect();
            let evaluated_args = evaluated_args?;

            // Check if the name refers to a Closure variable
            if let Some(EvalValue::Closure {
                name: fn_name,
                captured_args,
                remaining_arity,
            }) = ctx.get_variable(name)
            {
                let eval_args: Vec<EvalValue> = evaluated_args
                    .iter()
                    .cloned()
                    .map(EvalValue::from_value)
                    .collect();
                let result = crate::builtins::apply_closure_with_config(
                    fn_name,
                    captured_args,
                    *remaining_arity,
                    &eval_args,
                    ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: e,
                })?;
                return result
                    .into_value()
                    .map_err(|leak| ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "applying closure '{}' (still needs {} arg(s)) leaves a closure; \
                         finish the partial application before using the result as data",
                            leak.name, leak.remaining_arity
                        ),
                    });
            }

            // Try built-in first (with config for decrypt support)
            let eval_args: Vec<EvalValue> = evaluated_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            match crate::builtins::evaluate_builtin_with_config(name, &eval_args, ctx.config) {
                Ok(result) => result
                    .into_value()
                    .map_err(|leak| ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "{}(): produced a closure '{}' (still needs {} arg(s)); \
                         finish the partial application before using the result as data",
                            name, leak.name, leak.remaining_arity
                        ),
                    }),
                Err(_builtin_err) => {
                    // Try user-defined function
                    if let Some(user_fn) = ctx.user_functions.get(name) {
                        evaluate_user_function(user_fn, &evaluated_args, ctx)
                    } else {
                        // Keep as FunctionCall (may contain unresolved refs)
                        if evaluated_args.iter().all(is_static_value) {
                            Err(ParseError::InvalidExpression {
                                line: 0,
                                message: format!("Unknown function: {name}"),
                            })
                        } else {
                            Ok(Value::FunctionCall {
                                name: name.clone(),
                                args: evaluated_args,
                            })
                        }
                    }
                }
            }
        }
        Value::List(items) => {
            let evaluated: Result<Vec<Value>, ParseError> = items
                .into_iter()
                .map(|v| try_evaluate_fn_value(v, ctx))
                .collect();
            Ok(Value::List(evaluated?))
        }
        Value::Map(map) => {
            let evaluated: Result<IndexMap<String, Value>, ParseError> = map
                .into_iter()
                .map(|(k, v)| try_evaluate_fn_value(v, ctx).map(|ev| (k, ev)))
                .collect();
            Ok(Value::Map(evaluated?))
        }
        Value::Interpolation(parts) => {
            let evaluated: Result<Vec<crate::resource::InterpolationPart>, ParseError> = parts
                .into_iter()
                .map(|p| match p {
                    crate::resource::InterpolationPart::Expr(v) => {
                        try_evaluate_fn_value(v, ctx).map(crate::resource::InterpolationPart::Expr)
                    }
                    other => Ok(other),
                })
                .collect();
            Ok(Value::Interpolation(evaluated?))
        }
        other => Ok(other),
    }
}

fn parse_backend_block(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<BackendConfig, ParseError> {
    let mut inner = pair.into_inner();
    let backend_type = next_pair(&mut inner, "backend type", "backend block")?
        .as_str()
        .to_string();

    let mut attributes = HashMap::new();
    for attr_pair in inner {
        if attr_pair.as_rule() == Rule::attribute {
            let mut attr_inner = attr_pair.into_inner();
            let key = next_pair(&mut attr_inner, "attribute name", "backend block")?
                .as_str()
                .to_string();
            let value = parse_expression(
                next_pair(&mut attr_inner, "attribute value", "backend block")?,
                ctx,
            )?;
            attributes.insert(key, value);
        }
    }

    Ok(BackendConfig {
        backend_type,
        attributes,
    })
}

/// Parse an `upstream_state { source = "<dir>" }` expression.
///
/// The binding name comes from the enclosing `let` binding, so it's passed in
/// rather than extracted from the expression itself.
fn parse_upstream_state_expr(
    pair: pest::iterators::Pair<Rule>,
    binding_name: &str,
) -> Result<UpstreamState, ParseError> {
    let (block_line, _) = pair.as_span().start_pos().line_col();
    let inner = pair.into_inner();

    let mut source: Option<String> = None;
    for attr_pair in inner {
        if attr_pair.as_rule() != Rule::attribute {
            continue;
        }
        let (attr_line, _) = attr_pair.as_span().start_pos().line_col();
        let mut attr_inner = attr_pair.into_inner();
        let key = next_pair(
            &mut attr_inner,
            "attribute name",
            "upstream_state expression",
        )?
        .as_str()
        .to_string();
        let value_pair = next_pair(
            &mut attr_inner,
            "attribute value",
            "upstream_state expression",
        )?;
        match key.as_str() {
            "source" => {
                let value_text = value_pair.as_str().to_string();
                source = Some(extract_string_from_pair(value_pair).map_err(|_| {
                    ParseError::InvalidExpression {
                        line: attr_line,
                        message: format!(
                            "upstream_state '{}': 'source' must be a string literal, got: {}",
                            binding_name, value_text
                        ),
                    }
                })?);
            }
            other => {
                return Err(ParseError::InvalidExpression {
                    line: attr_line,
                    message: format!(
                        "unknown attribute '{}' in upstream_state '{}' expression",
                        other, binding_name
                    ),
                });
            }
        }
    }

    let source = source.ok_or_else(|| ParseError::InvalidExpression {
        line: block_line,
        message: format!(
            "upstream_state '{}' requires a 'source' attribute",
            binding_name
        ),
    })?;

    Ok(UpstreamState {
        binding: binding_name.to_string(),
        source: std::path::PathBuf::from(source),
    })
}

/// Extract a string value from a pair (expression -> pipe_expr -> primary -> string)
fn extract_string_from_pair(pair: pest::iterators::Pair<Rule>) -> Result<String, ParseError> {
    // Walk through expression -> pipe_expr -> primary -> string -> string_part -> string_literal
    fn find_string(pair: pest::iterators::Pair<Rule>) -> Option<String> {
        if pair.as_rule() == Rule::string_literal {
            return Some(pair.as_str().to_string());
        }
        if pair.as_rule() == Rule::single_quoted_content {
            return Some(unescape_single_quoted(pair.as_str()));
        }
        if pair.as_rule() == Rule::single_quoted_string {
            return pair
                .into_inner()
                .next()
                .map(|p| unescape_single_quoted(p.as_str()));
        }
        if pair.as_rule() == Rule::string {
            let mut result = String::new();
            for inner in pair.into_inner() {
                if let Some(s) = find_string(inner) {
                    result.push_str(&s);
                }
            }
            return Some(result);
        }
        if pair.as_rule() == Rule::string_part {
            for inner in pair.into_inner() {
                if let Some(s) = find_string(inner) {
                    return Some(s);
                }
            }
            return None;
        }
        for inner in pair.into_inner() {
            if let Some(s) = find_string(inner) {
                return Some(s);
            }
        }
        None
    }

    find_string(pair).ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "expected a string literal".to_string(),
    })
}

fn parse_anonymous_resource(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Resource, ParseError> {
    let body_pairs_clone = pair.clone().into_inner();
    let mut inner = pair.into_inner();

    let namespaced_type = next_pair(&mut inner, "resource type", "anonymous resource")?
        .as_str()
        .to_string();

    // Extract resource type from namespace (aws.s3_bucket -> s3_bucket)
    let parts: Vec<&str> = namespaced_type.split('.').collect();
    if parts.len() < 2 {
        return Err(ParseError::InvalidResourceType(namespaced_type));
    }

    let provider = parts[0];
    let resource_type = parts[1..].join(".");

    let attributes = parse_block_contents(inner, ctx)?;

    // Anonymous resources get an empty name that will be replaced by a hash-based
    // identifier computed from create-only properties after parsing.
    let resource_name = String::new();

    let mut attributes = attributes;
    attributes.insert("_type".to_string(), Value::String(namespaced_type.clone()));

    // Extract lifecycle block from attributes (it's a meta-argument, not a real attribute)
    let lifecycle = extract_lifecycle_config(&mut attributes);

    let id = ResourceId::with_provider(provider, resource_type, resource_name);
    record_string_literal_paths_for_resource(body_pairs_clone, &id, ctx);

    Ok(Resource {
        id,
        attributes: Expr::wrap_map(attributes),
        kind: ResourceKind::Real,
        lifecycle,
        prefixes: HashMap::new(),
        binding: None,
        dependency_bindings: BTreeSet::new(),
        module_source: None,
    })
}

/// Walk a parsed resource body and record every attribute whose value is a
/// plain quoted string literal (as opposed to a bare identifier or a
/// namespaced identifier). The parser collapses all three into
/// `Value::String`, so downstream diagnostics that need to distinguish them
/// (e.g. "expected an enum identifier, got a string literal") consult this
/// set. See #2094.
///
/// `pairs` is the `into_inner()` of the resource expression (starting with
/// the namespaced resource type); the type pair is skipped here.
fn record_string_literal_paths_for_resource(
    pairs: pest::iterators::Pairs<Rule>,
    resource_id: &ResourceId,
    ctx: &ParseContext,
) {
    let mut remaining = pairs;
    // Skip the leading namespaced type pair (e.g. `aws.s3_bucket`).
    let _ = remaining.next();
    record_string_literal_paths_in_block(remaining, resource_id, &[], ctx);
}

fn record_string_literal_paths_in_block(
    pairs: pest::iterators::Pairs<Rule>,
    resource_id: &ResourceId,
    base_path: &[String],
    ctx: &ParseContext,
) {
    // Track occurrence count per nested block name so a path like
    // `["rules", "0", "protocol"]` vs `["rules", "1", "protocol"]` stays
    // distinct — the schema validator walks list items by index.
    let mut nested_block_counts: HashMap<String, usize> = HashMap::new();

    for content_pair in pairs {
        let item = match content_pair.as_rule() {
            Rule::block_content => match content_pair.into_inner().next() {
                Some(inner) => inner,
                None => continue,
            },
            Rule::attribute => content_pair,
            _ => continue,
        };

        match item.as_rule() {
            Rule::attribute => {
                let mut attr_inner = item.into_inner();
                let key_pair = match attr_inner.next() {
                    Some(k) => k,
                    None => continue,
                };
                let key = match attribute_key_text(key_pair) {
                    Some(k) => k,
                    None => continue,
                };
                let value_pair = match attr_inner.next() {
                    Some(v) => v,
                    None => continue,
                };
                let mut chain = base_path.to_vec();
                chain.push(key);
                record_string_literals_in_expression(value_pair, resource_id, &chain, ctx);
            }
            Rule::nested_block => {
                let mut block_inner = item.into_inner();
                let name_pair = match block_inner.next() {
                    Some(n) => n,
                    None => continue,
                };
                let block_name = name_pair.as_str().to_string();
                let index = nested_block_counts.entry(block_name.clone()).or_insert(0);
                let idx = *index;
                *index += 1;
                let mut chain = base_path.to_vec();
                chain.push(block_name);
                chain.push(idx.to_string());
                record_string_literal_paths_in_block(block_inner, resource_id, &chain, ctx);
            }
            _ => {}
        }
    }
}

fn attribute_key_text(pair: pest::iterators::Pair<Rule>) -> Option<String> {
    match pair.as_rule() {
        Rule::identifier => Some(pair.as_str().to_string()),
        Rule::string => extract_plain_string_key(pair),
        _ => None,
    }
}

/// Best-effort extraction of the textual content of a `Rule::string` pair used
/// as an attribute key. Interpolation in keys is already disallowed elsewhere
/// in the parser; here we just read the literal segments and concatenate.
fn extract_plain_string_key(pair: pest::iterators::Pair<Rule>) -> Option<String> {
    let inner = pair.into_inner().next()?;
    match inner.as_rule() {
        Rule::single_quoted_string => inner.into_inner().next().map(|p| p.as_str().to_string()),
        Rule::double_quoted_string => {
            let mut out = String::new();
            for part in inner.into_inner() {
                if part.as_rule() == Rule::string_part {
                    for leaf in part.into_inner() {
                        if leaf.as_rule() == Rule::string_literal {
                            out.push_str(leaf.as_str());
                        }
                    }
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// Walk an `expression` pair and, if it unwraps to a plain string primary
/// (no function calls, no operators, no interpolation), record the attribute
/// at `chain` as coming from a quoted string literal. Also descends into
/// list/map literals to tag their elements.
fn record_string_literals_in_expression(
    pair: pest::iterators::Pair<Rule>,
    resource_id: &ResourceId,
    chain: &[String],
    ctx: &ParseContext,
) {
    // expression -> coalesce_expr -> pipe_expr -> compose_expr -> primary
    // Only "bare" chains (no pipes, no composes, no coalesce) can represent a
    // pure literal.
    let primary = match unwrap_to_primary(pair) {
        Some(p) => p,
        None => return,
    };
    let inner = match primary.into_inner().next() {
        Some(i) => i,
        None => return,
    };
    match inner.as_rule() {
        // A `Rule::string` primary that has no interpolation is the one shape
        // we tag; interpolated values like `"prefix-${name}"` do not count.
        Rule::string if is_plain_string_without_interpolation(&inner) => {
            ctx.record_string_literal(StringLiteralPath {
                resource_id: resource_id.clone(),
                attribute_chain: chain.to_vec(),
            });
        }
        Rule::list => {
            for (i, item) in inner.into_inner().enumerate() {
                let mut c = chain.to_vec();
                c.push(i.to_string());
                record_string_literals_in_expression(item, resource_id, &c, ctx);
            }
        }
        Rule::map => {
            let mut nested_counts: HashMap<String, usize> = HashMap::new();
            for entry in inner.into_inner() {
                match entry.as_rule() {
                    Rule::map_entry => {
                        let mut ei = entry.into_inner();
                        let key_pair = match ei.next() {
                            Some(k) => k,
                            None => continue,
                        };
                        let key = match attribute_key_text(key_pair) {
                            Some(k) => k,
                            None => continue,
                        };
                        let value_pair = match ei.next() {
                            Some(v) => v,
                            None => continue,
                        };
                        let mut c = chain.to_vec();
                        c.push(key);
                        record_string_literals_in_expression(value_pair, resource_id, &c, ctx);
                    }
                    Rule::nested_block => {
                        let mut bi = entry.into_inner();
                        let name_pair = match bi.next() {
                            Some(n) => n,
                            None => continue,
                        };
                        let block_name = name_pair.as_str().to_string();
                        let index = nested_counts.entry(block_name.clone()).or_insert(0);
                        let idx = *index;
                        *index += 1;
                        let mut c = chain.to_vec();
                        c.push(block_name);
                        c.push(idx.to_string());
                        record_string_literal_paths_in_block(bi, resource_id, &c, ctx);
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Walk `expression` (or any intermediate operator rule) down to the first
/// `primary`, returning `None` if any operator (|>, >>, ??) has more than
/// one operand — such chains are expressions, not pure literals.
fn unwrap_to_primary(pair: pest::iterators::Pair<Rule>) -> Option<pest::iterators::Pair<Rule>> {
    let rule = pair.as_rule();
    match rule {
        Rule::primary => Some(pair),
        Rule::expression | Rule::coalesce_expr | Rule::pipe_expr | Rule::compose_expr => {
            let mut inner = pair.into_inner();
            let first = inner.next()?;
            if inner.next().is_some() {
                // Second operand means an operator is applied — not a literal.
                return None;
            }
            unwrap_to_primary(first)
        }
        _ => None,
    }
}

fn is_plain_string_without_interpolation(pair: &pest::iterators::Pair<Rule>) -> bool {
    // Rule::string -> (single_quoted_string | double_quoted_string)
    let inner = match pair.clone().into_inner().next() {
        Some(i) => i,
        None => return false,
    };
    match inner.as_rule() {
        // Single-quoted strings do not support interpolation.
        Rule::single_quoted_string => true,
        Rule::double_quoted_string => {
            for part in inner.into_inner() {
                if part.as_rule() != Rule::string_part {
                    continue;
                }
                for leaf in part.into_inner() {
                    if leaf.as_rule() == Rule::interpolation {
                        return false;
                    }
                }
            }
            true
        }
        _ => false,
    }
}

/// Parse block contents (attributes, nested blocks, and local let bindings)
/// Nested blocks with the same name are collected into a list.
/// Local let bindings are resolved within the block scope and NOT included in
/// the returned attributes.
fn parse_block_contents(
    pairs: pest::iterators::Pairs<Rule>,
    ctx: &ParseContext,
) -> Result<IndexMap<String, Value>, ParseError> {
    // `IndexMap` so the order in which the user wrote attributes in the
    // .crn file flows all the way to `Resource.attributes` and to
    // `Value::Map` payloads — anything that re-renders attributes
    // (formatter, plan display, diagnostics) sees a stable order.
    let mut attributes: IndexMap<String, Value> = IndexMap::new();
    let mut nested_blocks: IndexMap<String, Vec<Value>> = IndexMap::new();

    // Local scope extends the parent context with block-scoped let bindings
    let mut local_ctx = ctx.clone();

    for content_pair in pairs {
        match content_pair.as_rule() {
            Rule::block_content => {
                let inner = first_inner(content_pair, "block content item", "block content")?;
                match inner.as_rule() {
                    Rule::local_binding => {
                        let mut binding_inner = inner.into_inner();
                        let name =
                            next_pair(&mut binding_inner, "binding name", "local let binding")?
                                .as_str()
                                .to_string();
                        let value = parse_expression(
                            next_pair(&mut binding_inner, "binding value", "local let binding")?,
                            &local_ctx,
                        )?;
                        // Add to local scope only, not to attributes
                        local_ctx.set_variable(name, value);
                    }
                    Rule::attribute => {
                        let mut attr_inner = inner.into_inner();
                        let key_pair =
                            next_pair(&mut attr_inner, "attribute name", "block content")?;
                        let key = extract_key_string(key_pair)?;
                        let value = parse_expression(
                            next_pair(&mut attr_inner, "attribute value", "block content")?,
                            &local_ctx,
                        )?;
                        attributes.insert(key, value);
                    }
                    Rule::nested_block => {
                        let mut block_inner = inner.into_inner();
                        let block_name = next_pair(&mut block_inner, "block name", "nested block")?
                            .as_str()
                            .to_string();

                        // Recursively parse nested block contents (supports arbitrary depth)
                        let block_attrs = parse_block_contents(block_inner, &local_ctx)?;

                        nested_blocks
                            .entry(block_name)
                            .or_default()
                            .push(Value::Map(block_attrs));
                    }
                    _ => {}
                }
            }
            Rule::attribute => {
                let mut attr_inner = content_pair.into_inner();
                let key_pair = next_pair(&mut attr_inner, "attribute name", "block content")?;
                let key = extract_key_string(key_pair)?;
                let value = parse_expression(
                    next_pair(&mut attr_inner, "attribute value", "block content")?,
                    &local_ctx,
                )?;
                attributes.insert(key, value);
            }
            _ => {}
        }
    }

    // Convert nested blocks to list attributes
    for (name, blocks) in nested_blocks {
        attributes.insert(name, Value::List(blocks));
    }

    Ok(attributes)
}

/// Extract lifecycle configuration from attributes.
/// The parser parses `lifecycle { ... }` as a nested block, which becomes
/// a List of Maps in attributes. We extract it and convert to LifecycleConfig.
fn extract_lifecycle_config(attributes: &mut IndexMap<String, Value>) -> LifecycleConfig {
    if let Some(Value::List(blocks)) = attributes.shift_remove("lifecycle") {
        // Take the first lifecycle block (there should only be one)
        if let Some(Value::Map(map)) = blocks.into_iter().next() {
            let force_delete = matches!(map.get("force_delete"), Some(Value::Bool(true)));
            let create_before_destroy =
                matches!(map.get("create_before_destroy"), Some(Value::Bool(true)));
            let prevent_destroy = matches!(map.get("prevent_destroy"), Some(Value::Bool(true)));
            return LifecycleConfig {
                force_delete,
                create_before_destroy,
                prevent_destroy,
            };
        }
    }
    LifecycleConfig::default()
}

fn parse_resource_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<Resource, ParseError> {
    let body_pairs_clone = pair.clone().into_inner();
    let mut inner = pair.into_inner();

    let namespaced_type = next_pair(&mut inner, "resource type", "resource expression")?
        .as_str()
        .to_string();

    // Extract resource type from namespace (aws.s3_bucket -> s3_bucket)
    let parts: Vec<&str> = namespaced_type.split('.').collect();
    if parts.len() < 2 {
        return Err(ParseError::InvalidResourceType(namespaced_type));
    }

    // First part is provider name, the rest is resource type
    let provider = parts[0];
    let resource_type = parts[1..].join(".");

    let mut attributes = parse_block_contents(inner, ctx)?;

    // All providers: use binding name as identifier.
    let resource_name = binding_name.to_string();

    // Extract lifecycle block from attributes (it's a meta-argument, not a real attribute)
    let lifecycle = extract_lifecycle_config(&mut attributes);

    attributes.insert("_type".to_string(), Value::String(namespaced_type.clone()));

    let id = ResourceId::with_provider(provider, resource_type, resource_name);
    record_string_literal_paths_for_resource(body_pairs_clone, &id, ctx);

    Ok(Resource {
        id,
        attributes: Expr::wrap_map(attributes),
        kind: ResourceKind::Real,
        lifecycle,
        prefixes: HashMap::new(),
        binding: Some(binding_name.to_string()),
        dependency_bindings: BTreeSet::new(),
        module_source: None,
    })
}

/// Parse a read resource expression (data source): read aws.s3_bucket { ... }
fn parse_read_resource_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
    binding_name: &str,
) -> Result<Resource, ParseError> {
    let body_pairs_clone = pair.clone().into_inner();
    let mut inner = pair.into_inner();

    let namespaced_type = next_pair(&mut inner, "resource type", "read resource expression")?
        .as_str()
        .to_string();

    // Extract resource type from namespace (aws.s3_bucket -> s3_bucket)
    let parts: Vec<&str> = namespaced_type.split('.').collect();
    if parts.len() < 2 {
        return Err(ParseError::InvalidResourceType(namespaced_type));
    }

    // First part is provider name, the rest is resource type
    let provider = parts[0];
    let resource_type = parts[1..].join(".");

    let mut attributes = parse_block_contents(inner, ctx)?;

    // All providers: use binding name as identifier.
    let resource_name = binding_name.to_string();

    // Extract lifecycle block from attributes (it's a meta-argument, not a real attribute)
    let lifecycle = extract_lifecycle_config(&mut attributes);

    attributes.insert("_type".to_string(), Value::String(namespaced_type.clone()));
    // Mark as data source
    attributes.insert("_data_source".to_string(), Value::Bool(true));

    let id = ResourceId::with_provider(provider, resource_type, resource_name);
    record_string_literal_paths_for_resource(body_pairs_clone, &id, ctx);

    Ok(Resource {
        id,
        attributes: Expr::wrap_map(attributes),
        kind: ResourceKind::DataSource,
        lifecycle,
        prefixes: HashMap::new(),
        binding: Some(binding_name.to_string()),
        dependency_bindings: BTreeSet::new(),
        module_source: None,
    })
}

/// Parse an expression. The result is a fully-reduced `Value`: any
/// closure that surfaces during evaluation surfaces here as a
/// parse-time error. Use [`parse_expression_eval`] in pipe/compose
/// paths where partial applications are legitimate intermediates.
fn parse_expression(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    let eval = parse_expression_eval(pair, ctx)?;
    eval.into_value()
        .map_err(|leak| ParseError::InvalidExpression {
            line: 0,
            message: format!(
                "expression evaluates to a closure '{}' (still needs {} arg(s)); finish the \
             partial application — closures are not valid as data",
                leak.name, leak.remaining_arity
            ),
        })
}

/// Parse an expression and return the raw `EvalValue`, preserving any
/// closure produced during partial application. Only the pipe/compose
/// paths and the let-binding RHS need this; everything else should
/// call [`parse_expression`] and let unfinished closures surface as
/// errors at the type boundary.
fn parse_expression_eval(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    let inner = first_inner(pair, "expression body", "expression")?;
    parse_coalesce_expr(inner, ctx)
}

fn parse_coalesce_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    let mut inner = pair.into_inner();
    let first = next_pair(&mut inner, "pipe expression", "coalesce expression")?;
    let value = parse_pipe_expr(first, ctx)?;

    // If there's a ?? right-hand side, check if left is an unresolved reference
    if let Some(rhs_pair) = inner.next() {
        let default = parse_pipe_expr(rhs_pair, ctx)?;
        match &value {
            EvalValue::User(Value::ResourceRef { .. }) => Ok(default),
            _ => Ok(value),
        }
    } else {
        Ok(value)
    }
}

fn parse_compose_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    let mut inner = pair.into_inner();
    let first = next_pair(&mut inner, "primary expression", "compose expression")?;
    let mut value = parse_primary_eval(first, ctx)?;

    // Collect remaining primaries for >> composition
    for rhs_pair in inner {
        let rhs = parse_primary_eval(rhs_pair, ctx)?;

        // Both sides must be Closures
        if !value.is_closure() {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: format!(
                    "left side of >> must be a Closure (partially applied function), got {}",
                    eval_type_name(&value)
                ),
            });
        }
        if !rhs.is_closure() {
            return Err(ParseError::InvalidExpression {
                line: 0,
                message: format!(
                    "right side of >> must be a Closure (partially applied function), got {}",
                    eval_type_name(&rhs)
                ),
            });
        }

        // Build a composed closure: __compose__ with the chain stored in captured_args
        // If the left side is already a __compose__, extend the chain; otherwise start a new one
        let functions = if let EvalValue::Closure {
            name,
            captured_args,
            ..
        } = &value
            && name == "__compose__"
        {
            let mut fns = captured_args.clone();
            fns.push(rhs);
            fns
        } else {
            vec![value, rhs]
        };

        value = EvalValue::closure("__compose__", functions, 1);
    }

    Ok(value)
}

fn parse_pipe_expr(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    let mut inner = pair.into_inner();
    let compose = next_pair(&mut inner, "compose expression", "pipe expression")?;
    let mut value = parse_compose_expr(compose, ctx)?;

    // Desugar pipe: `x |> f(args)` becomes `f(x, args)`
    for func_call_pair in inner {
        let mut fc_inner = func_call_pair.into_inner();
        let func_name = next_pair(&mut fc_inner, "function name", "pipe function call")?
            .as_str()
            .to_string();
        let extra_args: Result<Vec<Value>, ParseError> =
            fc_inner.map(|arg| parse_expression(arg, ctx)).collect();
        let extra_args = extra_args?;

        // Check if the pipe target is a Closure variable. The pipe
        // value (the running `value`) is appended last as an
        // EvalValue, so a closure carried in the binding can finish
        // applying through subsequent pipes.
        if let Some(EvalValue::Closure {
            name: fn_name,
            captured_args,
            remaining_arity,
        }) = ctx.get_variable(&func_name)
        {
            let mut all_args: Vec<EvalValue> = extra_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            all_args.push(value.clone());
            if all_args.iter().all(is_static_eval) {
                value = crate::builtins::apply_closure_with_config(
                    fn_name,
                    captured_args,
                    *remaining_arity,
                    &all_args,
                    ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: e,
                })?;
                continue;
            }
        }

        // Build args for the non-closure dispatch path. The running
        // pipe value must be a user-facing value here; a closure
        // would mean the user piped a partial application into a
        // non-closure-aware call, which we surface as a parse error.
        let pipe_value = match value {
            EvalValue::User(v) => v,
            EvalValue::Closure {
                ref name,
                remaining_arity,
                ..
            } => {
                return Err(ParseError::InvalidExpression {
                    line: 0,
                    message: format!(
                        "cannot pipe a closure '{}' (still needs {} arg(s)) into '{}' \
                         — finish the partial application first",
                        name, remaining_arity, func_name
                    ),
                });
            }
        };
        let mut args = extra_args;
        args.push(pipe_value);

        // Try to eagerly evaluate user-defined function calls
        if ctx.user_functions.contains_key(&func_name) && args.iter().all(is_static_value) {
            let user_fn = ctx.user_functions.get(&func_name).unwrap().clone();
            value = EvalValue::from_value(evaluate_user_function(&user_fn, &args, ctx)?);
        } else if let Some(arity) = crate::builtins::builtin_arity(&func_name) {
            // Eagerly evaluate partial application for builtin pipe targets
            if args.len() < arity && args.iter().all(is_static_value) {
                let eval_args: Vec<EvalValue> =
                    args.iter().cloned().map(EvalValue::from_value).collect();
                value = crate::builtins::evaluate_builtin_with_config(
                    &func_name, &eval_args, ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("{}(): {}", func_name, e),
                })?;
            } else {
                value = EvalValue::from_value(Value::FunctionCall {
                    name: func_name,
                    args,
                });
            }
        } else {
            value = EvalValue::from_value(Value::FunctionCall {
                name: func_name,
                args,
            });
        }
    }

    Ok(value)
}

fn parse_primary_eval(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<EvalValue, ParseError> {
    // For primary, get inner content; otherwise process directly
    let inner = if pair.as_rule() == Rule::primary {
        first_inner(pair, "value", "primary expression")?
    } else {
        pair
    };

    match inner.as_rule() {
        Rule::resource_expr => {
            // Resource expressions cannot be used as attribute values (only valid in top-level let bindings)
            Err(ParseError::InvalidExpression {
                line: 0,
                message: "Resource expressions can only be used in let bindings".to_string(),
            })
        }
        Rule::list => {
            let items: Result<Vec<Value>, ParseError> = inner
                .into_inner()
                .map(|item| parse_expression(item, ctx))
                .collect();
            Ok(EvalValue::from_value(Value::List(items?)))
        }
        Rule::map => {
            let mut map: IndexMap<String, Value> = IndexMap::new();
            let mut nested_blocks: IndexMap<String, Vec<Value>> = IndexMap::new();
            for entry in inner.into_inner() {
                match entry.as_rule() {
                    Rule::map_entry => {
                        let mut entry_inner = entry.into_inner();
                        let key_pair = next_pair(&mut entry_inner, "map key", "map entry")?;
                        let key = extract_key_string(key_pair)?;
                        let value = parse_expression(
                            next_pair(&mut entry_inner, "map value", "map entry")?,
                            ctx,
                        )?;
                        map.insert(key, value);
                    }
                    Rule::nested_block => {
                        let mut block_inner = entry.into_inner();
                        let block_name =
                            next_pair(&mut block_inner, "block name", "nested block in map")?
                                .as_str()
                                .to_string();
                        let block_attrs = parse_block_contents(block_inner, ctx)?;
                        nested_blocks
                            .entry(block_name)
                            .or_default()
                            .push(Value::Map(block_attrs));
                    }
                    _ => {}
                }
            }
            for (name, blocks) in nested_blocks {
                map.insert(name, Value::List(blocks));
            }
            Ok(EvalValue::from_value(Value::Map(map)))
        }
        Rule::namespaced_id => {
            // Namespaced identifier (e.g., aws.Region.ap_northeast_1)
            // or resource reference (e.g., bucket.name)
            // or arguments reference in module context (e.g., arguments.vpc_id)
            let full_str = inner.as_str();
            let parts: Vec<&str> = full_str.split('.').collect();

            if parts.len() == 2 {
                // Two-part identifier: could be resource reference or variable access
                if ctx.get_variable(parts[0]).is_some() && !ctx.is_resource_binding(parts[0]) {
                    // Variable exists but trying to access attribute on non-resource
                    Err(ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "'{}' is not a resource, cannot access attribute '{}'",
                            parts[0], parts[1]
                        ),
                    })
                } else if ctx.is_resource_binding(parts[0]) {
                    // Known resource binding: treat as resource reference
                    Ok(EvalValue::from_value(Value::resource_ref(
                        parts[0].to_string(),
                        parts[1].to_string(),
                        vec![],
                    )))
                } else {
                    // Unknown 2-part identifier: could be TypeName.value enum shorthand
                    // Will be resolved during schema validation
                    Ok(EvalValue::from_value(Value::String(format!(
                        "{}.{}",
                        parts[0], parts[1]
                    ))))
                }
            } else if ctx.is_resource_binding(parts[0]) {
                // 3+ part identifier where first part is a resource binding:
                // chained field access (e.g., web.network.vpc_id)
                Ok(EvalValue::from_value(Value::resource_ref(
                    parts[0].to_string(),
                    parts[1].to_string(),
                    parts[2..].iter().map(|s| s.to_string()).collect(),
                )))
            } else {
                // 3+ part identifier is a namespaced type (aws.Region.ap_northeast_1)
                Ok(EvalValue::from_value(Value::String(full_str.to_string())))
            }
        }
        Rule::boolean => {
            let b = inner.as_str() == "true";
            Ok(EvalValue::from_value(Value::Bool(b)))
        }
        Rule::float => {
            let f: f64 = inner
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: inner.line_col().0,
                    message: format!("invalid float literal: {e}"),
                })?;
            Ok(EvalValue::from_value(Value::Float(f)))
        }
        Rule::number => {
            let n: i64 = inner
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line: inner.line_col().0,
                    message: format!("integer literal out of range: {e}"),
                })?;
            Ok(EvalValue::from_value(Value::Int(n)))
        }
        Rule::string => parse_string_value(inner, ctx).map(EvalValue::from_value),
        Rule::function_call => {
            let mut fc_inner = inner.into_inner();
            let func_name = next_pair(&mut fc_inner, "function name", "function call")?
                .as_str()
                .to_string();
            let args: Result<Vec<Value>, ParseError> =
                fc_inner.map(|arg| parse_expression(arg, ctx)).collect();
            let args = args?;

            // Check if the name refers to a Closure variable (direct call on closure)
            if let Some(EvalValue::Closure {
                name: fn_name,
                captured_args,
                remaining_arity,
            }) = ctx.get_variable(&func_name)
                && args.iter().all(is_static_value)
            {
                let eval_args: Vec<EvalValue> =
                    args.iter().cloned().map(EvalValue::from_value).collect();
                return crate::builtins::apply_closure_with_config(
                    fn_name,
                    captured_args,
                    *remaining_arity,
                    &eval_args,
                    ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: e,
                });
            }

            // Try to eagerly evaluate user-defined function calls
            if ctx.user_functions.contains_key(&func_name) && args.iter().all(is_static_value) {
                let user_fn = ctx.user_functions.get(&func_name).unwrap().clone();
                return evaluate_user_function(&user_fn, &args, ctx).map(EvalValue::from_value);
            }

            // Eagerly evaluate partial application (fewer args than arity → Closure)
            if let Some(arity) = crate::builtins::builtin_arity(&func_name)
                && args.len() < arity
                && args.iter().all(is_static_value)
            {
                let eval_args: Vec<EvalValue> =
                    args.iter().cloned().map(EvalValue::from_value).collect();
                return crate::builtins::evaluate_builtin_with_config(
                    &func_name, &eval_args, ctx.config,
                )
                .map_err(|e| ParseError::InvalidExpression {
                    line: 0,
                    message: format!("{}(): {}", func_name, e),
                });
            }

            Ok(EvalValue::from_value(Value::FunctionCall {
                name: func_name,
                args,
            }))
        }
        Rule::variable_ref => {
            // variable_ref = { identifier ~ (field_access | index_access)* }
            // field_access = { "." ~ identifier }
            // index_access = { "[" ~ expression ~ "]" }
            let mut parts = inner.into_inner();
            let first_ident = next_pair(&mut parts, "identifier", "variable reference")?.as_str();

            // Collect all access steps (field or index)
            let access_steps: Vec<pest::iterators::Pair<Rule>> = parts.collect();

            if access_steps.is_empty() {
                // Simple variable reference (no access chain)
                match ctx.get_variable(first_ident) {
                    Some(val) => Ok(val.clone()),
                    None => Ok(EvalValue::from_value(Value::String(
                        first_ident.to_string(),
                    ))),
                }
            } else {
                // Build binding_name, attribute_name, and field_path from access steps.
                // Index access (e.g., [0] or ["key"]) composes the binding name.
                // Field access after the binding gives attribute_name and field_path.
                let mut binding_name = first_ident.to_string();
                let mut field_names: Vec<String> = Vec::new();
                let mut in_field_phase = false;

                for step in access_steps {
                    match step.as_rule() {
                        Rule::index_access => {
                            if in_field_phase {
                                // Index access after field access is not yet supported
                                // (e.g., a.b[0] — would need runtime list indexing)
                                return Err(ParseError::InvalidExpression {
                                    line: 0,
                                    message: "index access after field access is not supported"
                                        .to_string(),
                                });
                            }
                            // Parse the index expression
                            let index_expr_pair =
                                first_inner(step, "index expression", "index access")?;
                            let index_value = parse_expression(index_expr_pair, ctx)?;
                            // Compose the binding name: name[0] or name["key"]
                            match &index_value {
                                Value::Int(n) => {
                                    binding_name = format!("{}[{}]", binding_name, n);
                                }
                                Value::String(s) => {
                                    binding_name = format!("{}[\"{}\"]", binding_name, s);
                                }
                                other => {
                                    return Err(ParseError::InvalidExpression {
                                        line: 0,
                                        message: format!(
                                            "index access key must be an integer or string, got {:?}",
                                            other
                                        ),
                                    });
                                }
                            }
                        }
                        Rule::field_access => {
                            in_field_phase = true;
                            let field_ident =
                                first_inner(step, "field identifier", "field access")?;
                            field_names.push(field_ident.as_str().to_string());
                        }
                        _ => {}
                    }
                }

                if field_names.is_empty() {
                    // Index access only, no field access (e.g., subnets[0])
                    // Check if the composed binding name is a known variable
                    match ctx.get_variable(&binding_name) {
                        Some(val) => Ok(val.clone()),
                        None => {
                            // Return as ResourceRef with empty attribute_name
                            // (will be resolved later)
                            Ok(EvalValue::from_value(Value::resource_ref(
                                binding_name,
                                String::new(),
                                vec![],
                            )))
                        }
                    }
                } else {
                    let attribute_name = field_names.remove(0);
                    Ok(EvalValue::from_value(Value::resource_ref(
                        binding_name,
                        attribute_name,
                        field_names,
                    )))
                }
            }
        }
        Rule::if_expr => parse_if_value_expr(inner, ctx).map(EvalValue::from_value),
        Rule::expression => parse_expression_eval(inner, ctx),
        _ => Ok(EvalValue::from_value(Value::String(
            inner.as_str().to_string(),
        ))),
    }
}

fn parse_string_value(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    use crate::resource::InterpolationPart;

    // string = single_quoted_string | double_quoted_string
    let inner_pair = first_inner(pair, "string content", "string")?;

    if inner_pair.as_rule() == Rule::single_quoted_string {
        // Single-quoted: literal only, no interpolation
        let content = inner_pair
            .into_inner()
            .next()
            .map(|p| unescape_single_quoted(p.as_str()))
            .unwrap_or_default();
        return Ok(Value::String(content));
    }

    // Double-quoted string (original behavior)
    let mut parts: Vec<InterpolationPart> = Vec::new();
    let mut has_interpolation = false;

    for part in inner_pair.into_inner() {
        if part.as_rule() == Rule::string_part {
            let inner = first_inner(part, "string content", "string_part")?;
            match inner.as_rule() {
                Rule::string_literal => {
                    let s = unescape_string(inner.as_str());
                    parts.push(InterpolationPart::Literal(s));
                }
                Rule::interpolation => {
                    has_interpolation = true;
                    let expr_pair =
                        first_inner(inner, "interpolation expression", "interpolation")?;
                    let value = parse_expression(expr_pair, ctx)?;
                    parts.push(InterpolationPart::Expr(value));
                }
                _ => {}
            }
        }
    }

    if has_interpolation {
        Ok(Value::Interpolation(parts))
    } else {
        // No interpolation — collapse to a plain String
        let s = parts
            .into_iter()
            .map(|p| match p {
                InterpolationPart::Literal(s) => s,
                _ => unreachable!(),
            })
            .collect::<String>();
        Ok(Value::String(s))
    }
}

/// Handle escape sequences in string literals
fn unescape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                Some('$') => result.push('$'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Handle escape sequences in single-quoted string literals.
/// Only `\'` and `\\` are recognized as escape sequences.
fn unescape_single_quoted(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\'') => result.push('\''),
                Some('\\') => result.push('\\'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Build an `UndefinedIdentifier` error enriched with the best
/// did-you-mean suggestion and the sorted in-scope binding list.
fn undefined_identifier_error(
    known: &std::collections::HashSet<&str>,
    name: String,
    line: usize,
) -> ParseError {
    ParseError::undefined_identifier(name, line, known.iter().map(|s| s.to_string()).collect())
}

impl ParseError {
    /// Construct an `UndefinedIdentifier` error with did-you-mean suggestion
    /// and sorted in-scope binding list. `known_bindings` is the set of
    /// names that are in scope at the check site — they will be sorted
    /// and used to compute the close-match suggestion. See #2038.
    pub fn undefined_identifier(
        name: String,
        line: usize,
        known_bindings: Vec<String>,
    ) -> ParseError {
        let known_refs: Vec<&str> = known_bindings.iter().map(String::as_str).collect();
        let suggestion = crate::schema::suggest_similar_name(&name, &known_refs);
        let mut in_scope = known_bindings;
        in_scope.sort();
        ParseError::UndefinedIdentifier {
            name,
            line,
            suggestion,
            in_scope,
        }
    }
}

/// Resolve forward references after the full binding set is known.
///
/// During single-pass parsing, `identifier.member` forms where `identifier` is
/// not yet a known binding are stored as `String("identifier.member")`.
/// This function walks all resource attributes, module call arguments, and attribute
/// parameter values, converting matching strings to `ResourceRef`.
fn resolve_forward_references(
    resource_bindings: &HashMap<String, Resource>,
    resources: &mut [Resource],
    attribute_params: &mut [AttributeParameter],
    module_calls: &mut [ModuleCall],
    export_params: &mut [ExportParameter],
) {
    for resource in resources.iter_mut() {
        // In-place replace via `iter_mut`: avoids the O(n²) cost of
        // `shift_remove` + re-insert per key, and naturally preserves
        // the user-authored attribute order without a key-collection
        // round-trip. The placeholder is overwritten on the next line,
        // so its identity doesn't matter.
        for (_, expr) in resource.attributes.iter_mut() {
            let placeholder = Value::Bool(false);
            let value = std::mem::replace(&mut expr.0, placeholder);
            expr.0 = resolve_forward_ref_in_value(value, resource_bindings);
        }
    }
    for attr_param in attribute_params.iter_mut() {
        if let Some(value) = attr_param.value.take() {
            attr_param.value = Some(resolve_forward_ref_in_value(value, resource_bindings));
        }
    }
    for call in module_calls.iter_mut() {
        let keys: Vec<String> = call.arguments.keys().cloned().collect();
        for key in keys {
            if let Some(value) = call.arguments.remove(&key) {
                let resolved = resolve_forward_ref_in_value(value, resource_bindings);
                call.arguments.insert(key, resolved);
            }
        }
    }
    for export_param in export_params.iter_mut() {
        if let Some(value) = export_param.value.take() {
            export_param.value = Some(resolve_forward_ref_in_value(value, resource_bindings));
        }
    }
}

/// Recursively resolve forward references in a single Value.
///
/// Strings in `"name.member"` format where `name` is a known resource binding
/// are resolved to `ResourceRef`. This handles forward references that were
/// stored as strings during single-pass parsing.
fn resolve_forward_ref_in_value(
    value: Value,
    resource_bindings: &HashMap<String, Resource>,
) -> Value {
    match value {
        Value::String(ref s) => {
            // A dotted string like "vpc.vpc_id" or "vpc.attr.nested" may be a
            // forward reference that was stored as a string during single-pass
            // parsing. Resolve it to ResourceRef if the first segment is a known
            // resource binding. Parts after the second become field_path.
            let parts: Vec<&str> = s.splitn(3, '.').collect();
            if parts.len() >= 2 && resource_bindings.contains_key(parts[0]) {
                let field_path = parts
                    .get(2)
                    .map(|rest| rest.split('.').map(|s| s.to_string()).collect())
                    .unwrap_or_default();
                return Value::resource_ref(parts[0].to_string(), parts[1].to_string(), field_path);
            }
            value
        }
        Value::List(items) => Value::List(
            items
                .into_iter()
                .map(|v| resolve_forward_ref_in_value(v, resource_bindings))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.into_iter()
                .map(|(k, v)| (k, resolve_forward_ref_in_value(v, resource_bindings)))
                .collect(),
        ),
        Value::Interpolation(parts) => {
            use crate::resource::InterpolationPart;
            Value::Interpolation(
                parts
                    .into_iter()
                    .map(|p| match p {
                        InterpolationPart::Expr(v) => InterpolationPart::Expr(
                            resolve_forward_ref_in_value(v, resource_bindings),
                        ),
                        other => other,
                    })
                    .collect(),
            )
        }
        Value::FunctionCall { name, args } => Value::FunctionCall {
            name,
            args: args
                .into_iter()
                .map(|v| resolve_forward_ref_in_value(v, resource_bindings))
                .collect(),
        },
        other => other,
    }
}

/// Resolve resource references in a ParsedFile
/// This replaces ResourceRef values with the actual attribute values from referenced resources
pub fn resolve_resource_refs(parsed: &mut ParsedFile) -> Result<(), ParseError> {
    resolve_resource_refs_with_config(parsed, &ProviderContext::default())
}

/// Resolve resource references with the given parser configuration.
pub fn resolve_resource_refs_with_config(
    parsed: &mut ParsedFile,
    config: &ProviderContext,
) -> Result<(), ParseError> {
    // Save dependency bindings before resolution may change ResourceRef binding names.
    // This preserves direct dependencies that would be lost by recursive resolution
    // (e.g., tgw_attach.transit_gateway_id resolves to tgw.id, losing the tgw_attach dep).
    for resource in &mut parsed.resources {
        let deps = crate::deps::get_resource_dependencies(resource);
        if !deps.is_empty() {
            resource.dependency_bindings = deps.into_iter().collect();
        }
    }

    // Build a map of binding_name -> attributes for quick lookup
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in &parsed.resources {
        if let Some(ref binding_name) = resource.binding {
            // `binding_map` only needs key-based lookup, not source order
            // (callers consume it via `.get(name)` for ResourceRef
            // resolution), so the inner map stays `HashMap`.
            binding_map.insert(binding_name.clone(), resource.resolved_attributes());
        }
    }

    // Register argument parameters so they're recognized as valid bindings
    for arg in &parsed.arguments {
        binding_map.entry(arg.name.clone()).or_default();
    }

    // Register module call bindings so ResourceRefs to them are not rejected.
    // The actual attribute values will be resolved after module expansion.
    for call in &parsed.module_calls {
        if let Some(ref name) = call.binding_name {
            binding_map.entry(name.clone()).or_default();
        }
    }

    // Register upstream_state bindings so ResourceRefs to them are not rejected.
    // The actual attribute values will be resolved at plan time when the state file is loaded.
    for us in &parsed.upstream_states {
        binding_map.entry(us.binding.clone()).or_default();
    }

    // Resolve references in each resource. Keep `IndexMap` to preserve
    // the user's source order through resolution (#2222).
    for resource in &mut parsed.resources {
        let mut resolved_attrs: IndexMap<String, Expr> = IndexMap::new();

        for (key, expr) in &resource.attributes {
            let resolved = resolve_value_with_config(expr, &binding_map, config)?;
            resolved_attrs.insert(key.clone(), Expr(resolved));
        }

        resource.attributes = resolved_attrs;
    }

    // Resolve cross-file forward references in export_params.
    // During per-file parsing, "binding.attribute" strings from sibling files
    // remain as Value::String. Convert them to ResourceRef now that the full
    // binding map is available.
    let resource_bindings: HashMap<String, Resource> = parsed
        .resources
        .iter()
        .filter_map(|r| r.binding.as_ref().map(|b| (b.clone(), r.clone())))
        .collect();
    for export_param in &mut parsed.export_params {
        if let Some(value) = export_param.value.take() {
            export_param.value = Some(resolve_forward_ref_in_value(value, &resource_bindings));
        }
    }

    Ok(())
}

/// Every binding name declared in the merged `ParsedFile`: resources,
/// arguments, module calls, upstream states, imports, user functions,
/// variables, and for/if structural bindings.
///
/// This is the canonical answer to "is this identifier in scope?" for
/// directory-wide checks. The same set feeds [`check_identifier_scope`]
/// and the LSP borrows it (via `carina_lsp::diagnostics::checks`) to
/// keep diagnostic suggestions consistent with the CLI.
pub fn collect_known_bindings_merged(parsed: &ParsedFile) -> std::collections::HashSet<&str> {
    let mut known: std::collections::HashSet<&str> = std::collections::HashSet::new();
    known.extend(parsed.resources.iter().filter_map(|r| r.binding.as_deref())); // allow: direct — parser-internal, pre-expansion
    known.extend(parsed.arguments.iter().map(|a| a.name.as_str()));
    known.extend(
        parsed
            .module_calls
            .iter()
            .filter_map(|c| c.binding_name.as_deref()),
    );
    known.extend(parsed.upstream_states.iter().map(|u| u.binding.as_str()));
    known.extend(parsed.uses.iter().map(|i| i.alias.as_str()));
    known.extend(parsed.user_functions.keys().map(String::as_str));
    known.extend(parsed.variables.keys().map(String::as_str));
    known.extend(parsed.structural_bindings.iter().map(String::as_str));
    known
}

/// Directory-wide identifier-scope validation for a merged [`ParsedFile`].
///
/// Emits one flat list of `UndefinedIdentifier` errors covering:
///
/// - Every `ResourceRef` whose root binding is not in scope (roots in
///   resource attributes, attribute-parameter values, module-call
///   arguments, and export-parameter values).
/// - Every deferred for-expression iterable whose root is not in scope.
///
/// Errors are returned in a deterministic order: ResourceRef findings
/// first (in resource / attribute / module / export order), then
/// deferred-iterable findings. The caller (CLI `load_configuration_with_config`,
/// LSP analysis pipeline) just inspects the returned `Vec` — both
/// checks share the same `collect_known_bindings_merged` pass, so there
/// is no performance reason to split them at the callsite.
///
/// **This is the canonical entry point for "is this identifier in
/// scope?" checks.** Follow the #2104 rule: any new semantic check in
/// that family gets added *here*, not as a new sibling function.
pub fn check_identifier_scope(parsed: &ParsedFile) -> Vec<ParseError> {
    let known = collect_known_bindings_merged(parsed);
    let mut errors = Vec::new();
    accumulate_undefined_reference_errors(parsed, &known, &mut errors);
    accumulate_deferred_iterable_errors(parsed, &known, &mut errors);
    errors
}

fn accumulate_undefined_reference_errors(
    parsed: &ParsedFile,
    known: &std::collections::HashSet<&str>,
    errors: &mut Vec<ParseError>,
) {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut check = |value: &Value| {
        value.visit_refs(&mut |path| {
            let root = path.binding();
            let root_ident = root.split(['[', ']']).next().unwrap_or(root);
            if !known.contains(root_ident) && seen.insert(root_ident.to_string()) {
                errors.push(undefined_identifier_error(known, root_ident.to_string(), 0));
            }
        });
    };

    for resource in &parsed.resources {
        for expr in resource.attributes.values() {
            check(&expr.0);
        }
    }
    for attr in &parsed.attribute_params {
        if let Some(value) = &attr.value {
            check(value);
        }
    }
    for call in &parsed.module_calls {
        for v in call.arguments.values() {
            check(v);
        }
    }
    for export in &parsed.export_params {
        if let Some(value) = &export.value {
            check(value);
        }
    }
}

fn accumulate_deferred_iterable_errors(
    parsed: &ParsedFile,
    known: &std::collections::HashSet<&str>,
    errors: &mut Vec<ParseError>,
) {
    for d in &parsed.deferred_for_expressions {
        if !known.contains(d.iterable_binding.as_str()) {
            errors.push(undefined_identifier_error(
                known,
                d.iterable_binding.clone(),
                d.line,
            ));
        }
    }
}

fn resolve_value_with_config(
    value: &Value,
    binding_map: &HashMap<String, HashMap<String, Value>>,
    config: &ProviderContext,
) -> Result<Value, ParseError> {
    match value {
        Value::ResourceRef { path } => match binding_map.get(path.binding()) {
            Some(attributes) => match attributes.get(path.attribute()) {
                Some(attr_value) => {
                    // Recursively resolve in case the attribute itself is a reference
                    resolve_value_with_config(attr_value, binding_map, config)
                }
                None => {
                    // Attribute not found, keep as reference (might be resolved at runtime)
                    Ok(value.clone())
                }
            },
            None => Err(ParseError::UndefinedVariable(format!(
                "{}.{}",
                path.binding(),
                path.attribute()
            ))),
        },
        Value::List(items) => {
            let resolved: Result<Vec<Value>, ParseError> = items
                .iter()
                .map(|item| resolve_value_with_config(item, binding_map, config))
                .collect();
            Ok(Value::List(resolved?))
        }
        Value::Map(map) => {
            let mut resolved: IndexMap<String, Value> = IndexMap::new();
            for (k, v) in map {
                resolved.insert(
                    k.clone(),
                    resolve_value_with_config(v, binding_map, config)?,
                );
            }
            Ok(Value::Map(resolved))
        }
        Value::Interpolation(parts) => {
            use crate::resource::InterpolationPart;
            let resolved: Result<Vec<InterpolationPart>, ParseError> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Expr(v) => Ok(InterpolationPart::Expr(
                        resolve_value_with_config(v, binding_map, config)?,
                    )),
                    other => Ok(other.clone()),
                })
                .collect();
            Ok(Value::Interpolation(resolved?))
        }
        Value::FunctionCall { name, args } => {
            let resolved_args: Result<Vec<Value>, ParseError> = args
                .iter()
                .map(|a| resolve_value_with_config(a, binding_map, config))
                .collect();
            let resolved_args = resolved_args?;

            let all_args_resolved = resolved_args.iter().all(is_static_value);

            let eval_args: Vec<EvalValue> = resolved_args
                .iter()
                .cloned()
                .map(EvalValue::from_value)
                .collect();
            match crate::builtins::evaluate_builtin_with_config(name, &eval_args, config) {
                Ok(result) => result
                    .into_value()
                    .map_err(|leak| ParseError::InvalidExpression {
                        line: 0,
                        message: format!(
                            "{}(): produced a closure '{}' (still needs {} arg(s)); \
                         finish the partial application before using the result as data",
                            name, leak.name, leak.remaining_arity
                        ),
                    }),
                Err(e) => {
                    if all_args_resolved {
                        // All args are resolved but builtin failed — propagate the error
                        Err(ParseError::InvalidExpression {
                            line: 0,
                            message: format!("{}(): {}", name, e),
                        })
                    } else {
                        // Args contain unresolved refs — keep as FunctionCall for later resolution
                        Ok(Value::FunctionCall {
                            name: name.clone(),
                            args: resolved_args,
                        })
                    }
                }
            }
        }
        _ => Ok(value.clone()),
    }
}

/// Parse a .crn file and resolve resource references
pub fn parse_and_resolve(input: &str) -> Result<ParsedFile, ParseError> {
    let mut parsed = parse(input, &ProviderContext::default())?;
    resolve_resource_refs(&mut parsed)?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::InterpolationPart;

    #[test]
    fn parse_and_resolve_returns_value_only_no_closure() {
        // Issue #2230 acceptance criterion 3: `parse_and_resolve` must
        // never expose a closure to its caller. Type-system enforcement
        // makes the literal claim trivially true (`Value::Closure` does
        // not exist), so this test doubles as a smoke check that
        // legitimate partial-application expressions (data-last pipes
        // + builtin chaining) still parse and produce a `Value` tree
        // that no consumer needs to inspect for a closure case.
        let input = r#"
            let xs = ["a", "b", "c"]
            let joined = xs |> join("-")
        "#;
        let parsed = parse_and_resolve(input).expect("parse_and_resolve should succeed");
        let joined = parsed
            .variables
            .get("joined")
            .expect("joined binding present");
        // No `Closure` arm exists on `Value`, so the only way this
        // could fail is if the call survived as a `FunctionCall` —
        // also a valid `Value`, never a closure. The point of the
        // test is that the type contract holds: whatever shape this
        // is, downstream code does not have to consider closures.
        match joined {
            Value::String(_) | Value::FunctionCall { .. } => {}
            other => panic!("unexpected variant for `joined`: {other:?}"),
        }
    }

    #[test]
    fn unfinished_closure_in_let_binding_is_dropped() {
        // Issue #2230 acceptance criterion 2: a `let` binding holding
        // an unfinished partial application must not surface a closure
        // to the caller. The evaluator-internal `EvalValue::Closure`
        // is dropped at the lowering boundary; the binding name simply
        // does not appear in `ParsedFile.variables`.
        let input = r#"let f = join("-")"#;
        let parsed =
            parse_and_resolve(input).expect("partial application in let binding should parse");
        assert!(
            parsed.variables.get("f").is_none(),
            "closure binding must not survive into ParsedFile.variables"
        );
    }

    #[test]
    fn iter_all_resources_yields_direct_then_deferred() {
        let src = r#"
            provider test {
                source = 'x/y'
                version = '0.1'
                region = 'ap-northeast-1'
            }
            test.r.res {
                name = "direct"
            }
            for _, id in orgs.accounts {
                test.r.res {
                    name = id
                }
            }
        "#;
        let parsed = parse(src, &ProviderContext::default()).unwrap();

        let items: Vec<_> = parsed.iter_all_resources().collect();
        assert_eq!(items.len(), 2, "expected one direct + one deferred");

        assert!(matches!(items[0].0, ResourceContext::Direct));
        assert_eq!(
            items[0].1.get_attr("name"),
            Some(&Value::String("direct".to_string()))
        );

        assert!(matches!(items[1].0, ResourceContext::Deferred(_)));
    }

    #[test]
    fn parse_provider_block() {
        let input = r#"
            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_resource_with_namespaced_type() {
        let input = r#"
            let my_bucket = aws.s3_bucket {
                name = "my-bucket"
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert_eq!(resource.id.name_str(), "my_bucket"); // binding name becomes the resource ID
        assert_eq!(
            resource.get_attr("name"),
            Some(&Value::String("my-bucket".to_string()))
        );
        assert_eq!(
            resource.get_attr("region"),
            Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
        );
    }

    #[test]
    fn parse_multiple_resources() {
        let input = r#"
            let logs = aws.s3_bucket {
                name = "app-logs"
            }

            let data = aws.s3_bucket {
                name = "app-data"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 2);
        assert_eq!(result.resources[0].id.name_str(), "logs"); // binding name becomes the resource ID
        assert_eq!(result.resources[1].id.name_str(), "data");
    }

    #[test]
    fn parse_variable_and_resource() {
        let input = r#"
            let default_region = aws.Region.ap_northeast_1

            let my_bucket = aws.s3_bucket {
                name = "my-bucket"
                region = default_region
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("region"),
            Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
        );
    }

    #[test]
    fn parse_full_example() {
        let input = r#"
            # Provider configuration
            provider aws {
                region = aws.Region.ap_northeast_1
            }

            # Variables
            let versioning = true
            let retention_days = 90

            # Resources
            let app_logs = aws.s3_bucket {
                name = "my-app-logs"
                versioning = versioning
                expiration_days = retention_days
            }

            let app_data = aws.s3_bucket {
                name = "my-app-data"
                versioning = versioning
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.resources.len(), 2);
        assert_eq!(
            result.resources[0].get_attr("versioning"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            result.resources[0].get_attr("expiration_days"),
            Some(&Value::Int(90))
        );
    }

    #[test]
    fn function_call_is_parsed() {
        let input = r#"
            let my_bucket = aws.s3_bucket {
                name = env("SOME_VAR")
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::FunctionCall {
                name: "env".to_string(),
                args: vec![Value::String("SOME_VAR".to_string())],
            })
        );
    }

    #[test]
    fn parse_gcp_resource() {
        let input = r#"
            let my_bucket = gcp.storage.bucket {
                name = "my-gcp-bucket"
                location = gcp.Location.asia_northeast1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(result.resources[0].id.resource_type, "storage.bucket");
        assert_eq!(result.resources[0].id.provider, "gcp");
        // _provider attribute should NOT be set (provider identity is in ResourceId)
        assert!(!result.resources[0].attributes.contains_key("_provider"));
    }

    #[test]
    fn parse_anonymous_resource() {
        let input = r#"
            aws.s3_bucket {
                name = "my-anonymous-bucket"
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert_eq!(resource.id.name_str(), ""); // anonymous resources get empty name (computed later)
    }

    #[test]
    fn parse_mixed_resources() {
        let input = r#"
            # Anonymous resource
            aws.s3_bucket {
                name = "anonymous-bucket"
            }

            # Named resource
            let named = aws.s3_bucket {
                name = "named-bucket"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 2);
        assert_eq!(result.resources[0].id.name_str(), ""); // anonymous gets empty name
        assert_eq!(result.resources[1].id.name_str(), "named"); // binding name becomes the resource ID
    }

    #[test]
    fn parse_anonymous_resource_without_name_succeeds() {
        let input = r#"
            aws.s3_bucket {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.resources[0].id.name_str(), ""); // empty name, computed later
    }

    #[test]
    fn parse_resource_reference() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "my-bucket"
                region = aws.Region.ap_northeast_1
            }

            let policy = aws.s3_bucket_policy {
                name = "my-policy"
                bucket = bucket.name
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 2);

        // Before resolution, the attribute should be a ResourceRef
        let policy = &result.resources[1];
        assert_eq!(
            policy.get_attr("bucket"),
            Some(&Value::resource_ref(
                "bucket".to_string(),
                "name".to_string(),
                vec![]
            ))
        );
    }

    #[test]
    fn parse_and_resolve_resource_reference() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "my-bucket"
                region = aws.Region.ap_northeast_1
            }

            let policy = aws.s3_bucket_policy {
                name = "my-policy"
                bucket = bucket.name
                bucket_region = bucket.region
            }
        "#;

        let result = parse_and_resolve(input).unwrap();
        assert_eq!(result.resources.len(), 2);

        // After resolution, the attribute should be the actual value
        let policy = &result.resources[1];
        assert_eq!(
            policy.get_attr("bucket"),
            Some(&Value::String("my-bucket".to_string()))
        );
        assert_eq!(
            policy.get_attr("bucket_region"),
            Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
        );
    }

    #[test]
    fn parse_undefined_two_part_identifier_becomes_string() {
        // When a 2-part identifier references an unknown binding,
        // it becomes a String (e.g., "nonexistent.name") for later schema validation
        let input = r#"
            let policy = aws.s3_bucket_policy {
                name = "my-policy"
                bucket = nonexistent.name
            }
        "#;

        // Parsing succeeds - unknown identifiers become String
        let result = parse_and_resolve(input);
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(
            parsed.resources[0].get_attr("bucket"),
            Some(&Value::String("nonexistent.name".to_string()))
        );
    }

    #[test]
    fn parse_bare_identifier_becomes_string() {
        // When a bare identifier is not a known variable or binding,
        // it becomes a String for later schema validation (enum resolution)
        let input = r#"
            let vpc = awscc.ec2.Vpc {
                instance_tenancy = dedicated
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("instance_tenancy"),
            Some(&Value::String("dedicated".to_string()))
        );
    }

    #[test]
    fn resource_reference_preserves_namespaced_id() {
        // Ensure that aws.Region.ap_northeast_1 is NOT treated as a resource reference
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "my-bucket"
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("region"),
            Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
        );
    }

    #[test]
    fn namespaced_id_with_digit_segment() {
        // Enum values containing dots (e.g., "ipsec.1") should be parsed
        // as part of a namespaced_id when written as an identifier
        let input = r#"
            let gw = awscc.ec2.vpn_gateway {
                type = awscc.ec2.vpn_gateway.Type.ipsec.1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("type"),
            Some(&Value::String(
                "awscc.ec2.vpn_gateway.Type.ipsec.1".to_string()
            ))
        );
    }

    #[test]
    fn parse_nested_blocks_terraform_style() {
        let input = r#"
            let web_sg = aws.security_group {
                name        = "web-sg"
                region      = aws.Region.ap_northeast_1
                vpc         = "my-vpc"
                description = "Web server security group"

                ingress {
                    protocol  = "tcp"
                    from_port = 80
                    to_port   = 80
                    cidr      = "0.0.0.0/0"
                }

                ingress {
                    protocol  = "tcp"
                    from_port = 443
                    to_port   = 443
                    cidr      = "0.0.0.0/0"
                }

                egress {
                    protocol  = "-1"
                    from_port = 0
                    to_port   = 0
                    cidr      = "0.0.0.0/0"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);

        let sg = &result.resources[0];
        assert_eq!(sg.id.resource_type, "security_group");

        // Check ingress is a list with 2 items
        let ingress = sg.get_attr("ingress").unwrap();
        if let Value::List(items) = ingress {
            assert_eq!(items.len(), 2);

            // Check first ingress rule
            if let Value::Map(rule) = &items[0] {
                assert_eq!(
                    rule.get("protocol"),
                    Some(&Value::String("tcp".to_string()))
                );
                assert_eq!(rule.get("from_port"), Some(&Value::Int(80)));
            } else {
                panic!("Expected map for ingress rule");
            }
        } else {
            panic!("Expected list for ingress");
        }

        // Check egress is a list with 1 item
        let egress = sg.get_attr("egress").unwrap();
        if let Value::List(items) = egress {
            assert_eq!(items.len(), 1);
        } else {
            panic!("Expected list for egress");
        }
    }

    #[test]
    fn parse_list_syntax() {
        let input = r#"
            let rt = aws.route_table {
                name   = "public-rt"
                region = aws.Region.ap_northeast_1
                vpc    = "my-vpc"
                routes = [
                    { destination = "0.0.0.0/0", gateway = "my-igw" },
                    { destination = "10.0.0.0/8", gateway = "local" }
                ]
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);

        let rt = &result.resources[0];
        let routes = rt.get_attr("routes").unwrap();
        if let Value::List(items) = routes {
            assert_eq!(items.len(), 2);

            if let Value::Map(route) = &items[0] {
                assert_eq!(
                    route.get("destination"),
                    Some(&Value::String("0.0.0.0/0".to_string()))
                );
                assert_eq!(
                    route.get("gateway"),
                    Some(&Value::String("my-igw".to_string()))
                );
            } else {
                panic!("Expected map for route");
            }
        } else {
            panic!("Expected list for routes");
        }
    }

    #[test]
    fn parse_directory_module() {
        let input = r#"
            arguments {
                vpc_id: String
                enable_https: Bool = true
            }

            attributes {
                sg_id: String = web_sg.id
            }

            let web_sg = aws.security_group {
                name   = "web-sg"
                vpc_id = vpc_id
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();

        // Check arguments
        assert_eq!(result.arguments.len(), 2);
        assert_eq!(result.arguments[0].name, "vpc_id");
        assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
        assert!(result.arguments[0].default.is_none());

        assert_eq!(result.arguments[1].name, "enable_https");
        assert_eq!(result.arguments[1].type_expr, TypeExpr::Bool);
        assert_eq!(result.arguments[1].default, Some(Value::Bool(true)));

        // Check attribute params
        assert_eq!(result.attribute_params.len(), 1);
        assert_eq!(result.attribute_params[0].name, "sg_id");
        assert_eq!(result.attribute_params[0].type_expr, Some(TypeExpr::String));

        // Check resource has argument reference (lexically scoped)
        assert_eq!(result.resources.len(), 1);
        let sg = &result.resources[0];
        assert_eq!(
            sg.get_attr("vpc_id"),
            Some(&Value::resource_ref(
                "vpc_id".to_string(),
                String::new(),
                vec![]
            ))
        );
    }

    #[test]
    fn parse_use_expression() {
        let input = r#"
            let web_tier = use { source = "./modules/web_tier" }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.uses.len(), 1);
        assert_eq!(result.uses[0].path, "./modules/web_tier");
        assert_eq!(result.uses[0].alias, "web_tier");
    }

    #[test]
    fn parse_use_expression_requires_source() {
        let input = r#"
            let web_tier = use { }
        "#;

        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("source"),
            "error should mention missing source, got: {msg}"
        );
    }

    #[test]
    fn parse_use_expression_rejects_unknown_attribute() {
        let input = r#"
            let web_tier = use { source = "./x", bogus = "y" }
        "#;

        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("bogus"),
            "error should mention unexpected attribute, got: {msg}"
        );
    }

    // The `use` expression is only valid as a top-level `let` binding RHS.
    // The grammar previously accepted it in any primary-value position, which
    // produced silent evaluator failures (issue #2233). These tests pin the
    // grammar boundary: any non-let-RHS position must be a parse error.

    #[test]
    fn parse_use_expression_rejected_as_module_call_argument() {
        let input = r#"
            some_module {
              network = use { source = "./modules/network" }
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_err(),
            "use expression as module-call argument must be rejected, got: {result:?}"
        );
    }

    #[test]
    fn parse_use_expression_rejected_in_list() {
        let input = r#"
            let mods = [use { source = "./modules/a" }]
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_err(),
            "use expression inside a list must be rejected, got: {result:?}"
        );
    }

    #[test]
    fn parse_use_expression_rejected_in_if_branch() {
        let input = r#"
            let net = if true { use { source = "./a" } } else { use { source = "./b" } }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_err(),
            "use expression inside an if branch must be rejected, got: {result:?}"
        );
    }

    #[test]
    fn parse_use_expression_rejected_in_local_let() {
        // `local_binding` (block-scoped `let`) goes through `parse_expression`,
        // which has no `use_expr` handling. Must be a parse error, not silent failure.
        let input = r#"
            aws.s3.bucket {
              name = "my-bucket"
              let mod_x = use { source = "./modules/x" }
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_err(),
            "use expression inside a local let binding must be rejected, got: {result:?}"
        );
    }

    #[test]
    fn parse_generic_type_expressions() {
        let input = r#"
            arguments {
                ports: list(Int)
                tags: map(String)
                cidrs: list(String)
            }

            attributes {
                result: list(String) = items.ids
            }

            let items = aws.item {
                name = "test"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();

        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::List(Box::new(TypeExpr::Int))
        );
        assert_eq!(
            result.arguments[1].type_expr,
            TypeExpr::Map(Box::new(TypeExpr::String))
        );
        assert_eq!(
            result.arguments[2].type_expr,
            TypeExpr::List(Box::new(TypeExpr::String))
        );
        assert_eq!(
            result.attribute_params[0].type_expr,
            Some(TypeExpr::List(Box::new(TypeExpr::String)))
        );
        assert!(result.attribute_params[0].value.is_some());
    }

    #[test]
    fn parse_ref_type_expression() {
        let input = r#"
            arguments {
                vpc: aws.vpc
                enable_https: Bool = true
            }

            attributes {
                security_group_id: aws.security_group = web_sg.id
            }

            let web_sg = aws.security_group {
                name   = "web-sg"
                vpc_id = vpc
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();

        // Check ref type argument
        assert_eq!(result.arguments[0].name, "vpc");
        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("aws", "vpc"))
        );
        assert!(result.arguments[0].default.is_none());

        // Check ref type attribute param
        assert_eq!(result.attribute_params[0].name, "security_group_id");
        assert_eq!(
            result.attribute_params[0].type_expr,
            Some(TypeExpr::Ref(ResourceTypePath::new(
                "aws",
                "security_group"
            )))
        );
    }

    #[test]
    fn parse_ref_type_with_nested_resource_type() {
        let input = r#"
            arguments {
                sg: aws.security_group
                rule: aws.security_group.ingress_rule
            }

            attributes {
                out: String = sg.name
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();

        // Single-level resource type
        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("aws", "security_group"))
        );

        // Nested resource type (security_group.ingress_rule)
        assert_eq!(
            result.arguments[1].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("aws", "security_group.ingress_rule"))
        );
    }

    #[test]
    fn parse_struct_type_expression() {
        let input = r#"
            exports {
                accounts: struct {
                    registry_prod: AwsAccountId,
                    registry_dev: AwsAccountId,
                } = {
                    registry_prod = "111111111111"
                    registry_dev  = "222222222222"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.export_params.len(), 1);
        let ep = &result.export_params[0];
        assert_eq!(ep.name, "accounts");
        let expected = TypeExpr::Struct {
            fields: vec![
                (
                    "registry_prod".to_string(),
                    TypeExpr::Simple("aws_account_id".to_string()),
                ),
                (
                    "registry_dev".to_string(),
                    TypeExpr::Simple("aws_account_id".to_string()),
                ),
            ],
        };
        assert_eq!(ep.type_expr, Some(expected));
    }

    #[test]
    fn parse_struct_type_nested_in_list_and_map() {
        let input = r#"
            arguments {
                items: list(struct { name: String, value: Int })
                registry: map(struct { arn: String, id: String })
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::List(Box::new(TypeExpr::Struct {
                fields: vec![
                    ("name".to_string(), TypeExpr::String),
                    ("value".to_string(), TypeExpr::Int),
                ],
            }))
        );
        assert_eq!(
            result.arguments[1].type_expr,
            TypeExpr::Map(Box::new(TypeExpr::Struct {
                fields: vec![
                    ("arn".to_string(), TypeExpr::String),
                    ("id".to_string(), TypeExpr::String),
                ],
            }))
        );
    }

    #[test]
    fn parse_struct_type_rejects_duplicate_field_name() {
        let input = r#"
            exports {
                x: struct { a: String, a: Int } = { a = "hi" }
            }
        "#;
        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("duplicate field name 'a'"),
            "expected duplicate-name error, got: {msg}"
        );
    }

    #[test]
    fn struct_type_expr_display_renders_with_braces() {
        let t = TypeExpr::Struct {
            fields: vec![
                ("name".to_string(), TypeExpr::String),
                ("value".to_string(), TypeExpr::Int),
            ],
        };
        assert_eq!(t.to_string(), "struct { name: String, value: Int }");

        let empty = TypeExpr::Struct { fields: vec![] };
        assert_eq!(empty.to_string(), "struct {}");
    }

    #[test]
    fn struct_type_expr_roundtrips_through_serde_json() {
        let t = TypeExpr::Struct {
            fields: vec![
                ("name".to_string(), TypeExpr::String),
                ("value".to_string(), TypeExpr::Int),
            ],
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: TypeExpr = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn parse_attributes_without_type_annotation() {
        let input = r#"
            attributes {
                security_group = sg.id
            }

            let sg = aws.security_group {
                name = "web-sg"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();

        assert_eq!(result.attribute_params.len(), 1);
        assert_eq!(result.attribute_params[0].name, "security_group");
        assert_eq!(result.attribute_params[0].type_expr, None);
        assert!(result.attribute_params[0].value.is_some());
    }

    #[test]
    fn parse_attributes_mixed_typed_and_untyped() {
        let input = r#"
            attributes {
                vpc_id: awscc.ec2.VpcId = vpc.vpc_id
                security_group = sg.id
                subnet_ids: list(String) = subnets.ids
            }

            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }

            let sg = aws.security_group {
                name = "web-sg"
            }

            let subnets = aws.subnet {
                vpc_id = vpc.vpc_id
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();

        assert_eq!(result.attribute_params.len(), 3);

        // Explicit type
        assert_eq!(result.attribute_params[0].name, "vpc_id");
        assert!(result.attribute_params[0].type_expr.is_some());
        assert!(result.attribute_params[0].value.is_some());

        // No type annotation
        assert_eq!(result.attribute_params[1].name, "security_group");
        assert_eq!(result.attribute_params[1].type_expr, None);
        assert!(result.attribute_params[1].value.is_some());

        // Explicit type
        assert_eq!(result.attribute_params[2].name, "subnet_ids");
        assert_eq!(
            result.attribute_params[2].type_expr,
            Some(TypeExpr::List(Box::new(TypeExpr::String)))
        );
        assert!(result.attribute_params[2].value.is_some());
    }

    #[test]
    fn resource_type_path_parse() {
        // Simple resource type
        let path = ResourceTypePath::parse("aws.vpc").unwrap();
        assert_eq!(path.provider, "aws");
        assert_eq!(path.resource_type, "vpc");

        // Nested resource type
        let path2 = ResourceTypePath::parse("aws.security_group.ingress_rule").unwrap();
        assert_eq!(path2.provider, "aws");
        assert_eq!(path2.resource_type, "security_group.ingress_rule");

        // Invalid (single component)
        assert!(ResourceTypePath::parse("vpc").is_none());
    }

    #[test]
    fn resource_type_path_display() {
        let path = ResourceTypePath::new("aws", "vpc");
        assert_eq!(path.to_string(), "aws.vpc");

        let path2 = ResourceTypePath::new("aws", "security_group.ingress_rule");
        assert_eq!(path2.to_string(), "aws.security_group.ingress_rule");
    }

    #[test]
    fn type_expr_display_with_ref() {
        assert_eq!(TypeExpr::String.to_string(), "String");
        assert_eq!(TypeExpr::Bool.to_string(), "Bool");
        assert_eq!(TypeExpr::Int.to_string(), "Int");
        assert_eq!(
            TypeExpr::List(Box::new(TypeExpr::String)).to_string(),
            "list(String)"
        );
        assert_eq!(
            TypeExpr::Ref(ResourceTypePath::new("aws", "vpc")).to_string(),
            "aws.vpc"
        );
    }

    #[test]
    fn parse_float_literal() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "test"
                weight = 2.5
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("weight"),
            Some(&Value::Float(2.5))
        );
    }

    #[test]
    fn parse_negative_float_literal() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "test"
                offset = -0.5
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("offset"),
            Some(&Value::Float(-0.5))
        );
    }

    #[test]
    fn type_expr_display_float() {
        assert_eq!(TypeExpr::Float.to_string(), "Float");
    }

    #[test]
    fn type_expr_display_primitives_are_pascal_case() {
        assert_eq!(TypeExpr::String.to_string(), "String");
        assert_eq!(TypeExpr::Int.to_string(), "Int");
        assert_eq!(TypeExpr::Bool.to_string(), "Bool");
        assert_eq!(TypeExpr::Float.to_string(), "Float");
        assert_eq!(
            TypeExpr::List(Box::new(TypeExpr::Int)).to_string(),
            "list(Int)"
        );
        assert_eq!(
            TypeExpr::Map(Box::new(TypeExpr::String)).to_string(),
            "map(String)"
        );
    }

    #[test]
    fn parse_backend_block() {
        let input = r#"
            backend s3 {
                bucket      = "my-carina-state"
                key         = "infra/prod/carina.crnstate"
                region      = aws.Region.ap_northeast_1
                encrypt     = true
                auto_create = true
            }

            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();

        // Check backend
        assert!(result.backend.is_some());
        let backend = result.backend.unwrap();
        assert_eq!(backend.backend_type, "s3");
        assert_eq!(
            backend.attributes.get("bucket"),
            Some(&Value::String("my-carina-state".to_string()))
        );
        assert_eq!(
            backend.attributes.get("key"),
            Some(&Value::String("infra/prod/carina.crnstate".to_string()))
        );
        assert_eq!(
            backend.attributes.get("region"),
            Some(&Value::String("aws.Region.ap_northeast_1".to_string()))
        );
        assert_eq!(backend.attributes.get("encrypt"), Some(&Value::Bool(true)));
        assert_eq!(
            backend.attributes.get("auto_create"),
            Some(&Value::Bool(true))
        );

        // Check provider
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_backend_block_with_resources() {
        let input = r#"
            backend s3 {
                bucket = "my-state"
                key    = "prod/carina.state"
                region = aws.Region.ap_northeast_1
            }

            provider aws {
                region = aws.Region.ap_northeast_1
            }

            aws.s3_bucket {
                name       = "my-state"
                versioning = "Enabled"
            }

            aws.ec2.Vpc {
                name       = "main-vpc"
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();

        assert!(result.backend.is_some());
        let backend = result.backend.unwrap();
        assert_eq!(backend.backend_type, "s3");
        assert_eq!(
            backend.attributes.get("bucket"),
            Some(&Value::String("my-state".to_string()))
        );

        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.resources.len(), 2);
    }

    #[test]
    fn parse_read_resource_expr() {
        let input = r#"
            let existing = read aws.s3_bucket {
                name = "my-existing-bucket"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert_eq!(resource.id.name_str(), "existing"); // binding name becomes the resource ID
        assert!(resource.is_data_source());
        assert_eq!(resource.get_attr("_data_source"), Some(&Value::Bool(true)));
    }

    #[test]
    fn parse_read_resource_without_name_uses_binding() {
        let input = r#"
            let existing = read aws.s3_bucket {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.resources[0].id.name_str(), "existing"); // binding name
    }

    #[test]
    fn parse_read_with_regular_resources() {
        let input = r#"
            # Read existing bucket (data source)
            let existing_bucket = read aws.s3_bucket {
                name = "existing-bucket"
            }

            # Create new bucket that depends on reading the existing one
            let new_bucket = aws.s3_bucket {
                name = "new-bucket"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 2);

        // First resource is read-only (data source)
        assert!(result.resources[0].is_data_source());
        assert_eq!(result.resources[0].id.name_str(), "existing_bucket"); // binding name

        // Second resource is a regular resource
        assert!(!result.resources[1].is_data_source());
        assert_eq!(result.resources[1].id.name_str(), "new_bucket"); // binding name
    }

    #[test]
    fn parse_lifecycle_force_delete() {
        let input = r#"
            let bucket = awscc.s3_bucket {
                bucket_name = "my-bucket"
                lifecycle {
                    force_delete = true
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.resource_type, "s3_bucket");
        assert!(resource.lifecycle.force_delete);
        // lifecycle should NOT appear in attributes
        assert!(!resource.attributes.contains_key("lifecycle"));
    }

    #[test]
    fn parse_lifecycle_default_when_absent() {
        let input = r#"
            let bucket = awscc.s3_bucket {
                bucket_name = "my-bucket"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert!(!result.resources[0].lifecycle.force_delete);
        assert!(!result.resources[0].lifecycle.prevent_destroy);
    }

    #[test]
    fn parse_lifecycle_anonymous_resource() {
        let input = r#"
            awscc.s3_bucket {
                bucket_name = "my-bucket"
                lifecycle {
                    force_delete = true
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert!(result.resources[0].lifecycle.force_delete);
        assert!(!result.resources[0].attributes.contains_key("lifecycle"));
    }

    /// Regression test for issue #146: anonymous AWSCC resources should not have
    /// a spurious "name" attribute injected into the attributes map.
    #[test]
    fn anonymous_resource_no_spurious_name_attribute() {
        let input = r#"
            awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.name_str(), ""); // anonymous → empty name
        // "name" must NOT appear in attributes unless the user explicitly wrote it
        assert!(
            !resource.attributes.contains_key("name"),
            "Anonymous AWSCC resource should not have 'name' in attributes, but found: {:?}",
            resource.get_attr("name")
        );
    }

    /// Regression test for issue #146: let-bound AWSCC resources should not have
    /// a spurious "name" attribute injected by the parser.
    #[test]
    fn let_bound_resource_no_spurious_name_attribute() {
        let input = r#"
            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert_eq!(resource.id.name_str(), "vpc"); // binding name → resource name
        // "name" must NOT appear in attributes (it's only the id.name, not an attribute)
        assert!(
            !resource.attributes.contains_key("name"),
            "Let-bound AWSCC resource should not have 'name' in attributes, but found: {:?}",
            resource.get_attr("name")
        );
    }

    #[test]
    fn parse_lifecycle_create_before_destroy() {
        let input = r#"
            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
                lifecycle {
                    create_before_destroy = true
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert!(resource.lifecycle.create_before_destroy);
        assert!(!resource.lifecycle.force_delete);
        assert!(!resource.attributes.contains_key("lifecycle"));
    }

    #[test]
    fn parse_lifecycle_both_force_delete_and_create_before_destroy() {
        let input = r#"
            let bucket = awscc.s3_bucket {
                bucket_name = "my-bucket"
                lifecycle {
                    force_delete = true
                    create_before_destroy = true
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);

        let resource = &result.resources[0];
        assert!(resource.lifecycle.force_delete);
        assert!(resource.lifecycle.create_before_destroy);
        assert!(!resource.attributes.contains_key("lifecycle"));
    }

    #[test]
    fn parse_block_syntax_inside_map() {
        let input = r#"
            let role = awscc.iam.role {
                assume_role_policy_document = {
                    version = "2012-10-17"
                    statement {
                        effect    = "Allow"
                        principal = { service = "lambda.amazonaws.com" }
                        action    = "sts:AssumeRole"
                    }
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);

        let role = &result.resources[0];
        let doc = role.get_attr("assume_role_policy_document").unwrap();
        if let Value::Map(map) = doc {
            assert_eq!(
                map.get("version"),
                Some(&Value::String("2012-10-17".to_string()))
            );
            // statement block becomes a list with one element
            let statement = map.get("statement").unwrap();
            if let Value::List(stmts) = statement {
                assert_eq!(stmts.len(), 1);
                if let Value::Map(stmt) = &stmts[0] {
                    assert_eq!(
                        stmt.get("effect"),
                        Some(&Value::String("Allow".to_string()))
                    );
                    assert_eq!(
                        stmt.get("action"),
                        Some(&Value::String("sts:AssumeRole".to_string()))
                    );
                } else {
                    panic!("Expected map for statement");
                }
            } else {
                panic!("Expected list for statement");
            }
        } else {
            panic!("Expected map for assume_role_policy_document");
        }
    }

    #[test]
    fn parse_multiple_blocks_inside_map() {
        let input = r#"
            let role = awscc.iam.role {
                policy_document = {
                    version = "2012-10-17"
                    statement {
                        effect = "Allow"
                        action = "s3:GetObject"
                    }
                    statement {
                        effect = "Deny"
                        action = "s3:DeleteObject"
                    }
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let role = &result.resources[0];
        let doc = role.get_attr("policy_document").unwrap();
        if let Value::Map(map) = doc {
            let statement = map.get("statement").unwrap();
            if let Value::List(stmts) = statement {
                assert_eq!(stmts.len(), 2);
            } else {
                panic!("Expected list for statement");
            }
        } else {
            panic!("Expected map for policy_document");
        }
    }

    #[test]
    fn parse_list_syntax_inside_map_still_works() {
        // Backward compatibility: list literal syntax still works
        let input = r#"
            let role = awscc.iam.role {
                assume_role_policy_document = {
                    version = "2012-10-17"
                    statement = [
                        {
                            effect    = "Allow"
                            principal = { service = "lambda.amazonaws.com" }
                            action    = "sts:AssumeRole"
                        }
                    ]
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let role = &result.resources[0];
        let doc = role.get_attr("assume_role_policy_document").unwrap();
        if let Value::Map(map) = doc {
            let statement = map.get("statement").unwrap();
            if let Value::List(stmts) = statement {
                assert_eq!(stmts.len(), 1);
            } else {
                panic!("Expected list for statement");
            }
        } else {
            panic!("Expected map for assume_role_policy_document");
        }
    }

    #[test]
    fn parse_deeply_nested_blocks() {
        // Test nested blocks at depth 2: resource { outer { inner { ... } } }
        let input = r#"
            let r = aws.test.resource {
                outer {
                    inner {
                        leaf = "value"
                    }
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let r = &result.resources[0];

        let outer = r.get_attr("outer").unwrap();
        if let Value::List(outer_items) = outer {
            assert_eq!(outer_items.len(), 1);
            if let Value::Map(outer_map) = &outer_items[0] {
                let inner = outer_map.get("inner").unwrap();
                if let Value::List(inner_items) = inner {
                    assert_eq!(inner_items.len(), 1);
                    if let Value::Map(inner_map) = &inner_items[0] {
                        assert_eq!(
                            inner_map.get("leaf"),
                            Some(&Value::String("value".to_string()))
                        );
                    } else {
                        panic!("Expected map for inner block");
                    }
                } else {
                    panic!("Expected list for inner");
                }
            } else {
                panic!("Expected map for outer block");
            }
        } else {
            panic!("Expected list for outer");
        }
    }

    #[test]
    fn parse_nested_block_in_map() {
        // Test nested block inside map value: attr = { block { ... } }
        let input = r#"
            let role = aws.iam.Role {
                policy_document = {
                    statement {
                        effect = "Allow"
                        action = "s3:GetObject"
                    }
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let role = &result.resources[0];

        let doc = role.get_attr("policy_document").unwrap();
        if let Value::Map(map) = doc {
            let statement = map.get("statement").unwrap();
            if let Value::List(items) = statement {
                assert_eq!(items.len(), 1);
                if let Value::Map(s) = &items[0] {
                    assert_eq!(s.get("effect"), Some(&Value::String("Allow".to_string())));
                } else {
                    panic!("Expected map for statement");
                }
            } else {
                panic!("Expected list for statement");
            }
        } else {
            panic!("Expected map for policy_document");
        }
    }

    #[test]
    fn test_find_resource_by_attr() {
        let input = r#"
            aws.s3.Bucket {
                bucket = "my-bucket"
            }
            aws.s3.Bucket {
                bucket = "other-bucket"
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();

        assert!(
            parsed
                .find_resource_by_attr("s3.Bucket", "bucket", "my-bucket")
                .is_some()
        );
        assert!(
            parsed
                .find_resource_by_attr("s3.Bucket", "bucket", "other-bucket")
                .is_some()
        );
        assert!(
            parsed
                .find_resource_by_attr("s3.Bucket", "bucket", "no-such")
                .is_none()
        );
        assert!(
            parsed
                .find_resource_by_attr("ec2.Vpc", "bucket", "my-bucket")
                .is_none()
        );
    }

    #[test]
    fn parse_integer_overflow_returns_error() {
        // i64::MAX is 9223372036854775807; one more should fail
        let input = r#"
provider aws {
    region = aws.Region.ap_northeast_1
}

aws.s3.Bucket {
    name = "test"
    count = 99999999999999999999
}
"#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("integer literal out of range"),
            "expected 'integer literal out of range' error, got: {err}"
        );
    }

    #[test]
    fn pipe_operator_desugars_to_function_call() {
        let input = r#"
            let x = "hello" |> upper()
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        // "hello" |> upper() desugars to upper("hello")
        assert_eq!(
            result.variables.get("x"),
            Some(&Value::FunctionCall {
                name: "upper".to_string(),
                args: vec![Value::String("hello".to_string())],
            })
        );
    }

    #[test]
    fn pipe_operator_in_attribute_desugars() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "test" |> lower()
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::FunctionCall {
                name: "lower".to_string(),
                args: vec![Value::String("test".to_string())],
            })
        );
    }

    #[test]
    fn join_function_call_parsed() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = join("-", ["a", "b", "c"])
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        // At parse time, function calls remain as FunctionCall values
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::FunctionCall {
                name: "join".to_string(),
                args: vec![
                    Value::String("-".to_string()),
                    Value::List(vec![
                        Value::String("a".to_string()),
                        Value::String("b".to_string()),
                        Value::String("c".to_string()),
                    ]),
                ],
            })
        );
    }

    #[test]
    fn pipe_with_join_parsed() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = ["a", "b", "c"] |> join("-")
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        // ["a", "b", "c"] |> join("-") desugars to join("-", ["a", "b", "c"])
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::FunctionCall {
                name: "join".to_string(),
                args: vec![
                    Value::String("-".to_string()),
                    Value::List(vec![
                        Value::String("a".to_string()),
                        Value::String("b".to_string()),
                        Value::String("c".to_string()),
                    ]),
                ],
            })
        );
    }

    #[test]
    fn join_with_multiple_pipes() {
        // Chain: value |> f1(args) |> f2(args)
        let input = r#"
            let x = ["a", "b"] |> join("-") |> upper()
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        // Pipe chaining: ["a", "b"] |> join("-") |> upper()
        // => upper(join("-", ["a", "b"]))
        assert_eq!(
            result.variables.get("x"),
            Some(&Value::FunctionCall {
                name: "upper".to_string(),
                args: vec![Value::FunctionCall {
                    name: "join".to_string(),
                    args: vec![
                        Value::String("-".to_string()),
                        Value::List(vec![
                            Value::String("a".to_string()),
                            Value::String("b".to_string()),
                        ]),
                    ],
                }],
            })
        );
    }

    #[test]
    fn function_call_with_no_args() {
        let input = r#"
            let x = foo()
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.variables.get("x"),
            Some(&Value::FunctionCall {
                name: "foo".to_string(),
                args: vec![],
            })
        );
    }

    #[test]
    fn join_resolved_during_resource_ref_resolution() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = join("-", ["my", "bucket", "name"])
            }
        "#;
        let mut result = parse(input, &ProviderContext::default()).unwrap();
        resolve_resource_refs(&mut result).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("my-bucket-name".to_string()))
        );
    }

    #[test]
    fn pipe_join_resolved_during_resource_ref_resolution() {
        let input = r#"
            let bucket = aws.s3_bucket {
                name = ["my", "bucket"] |> join("-")
            }
        "#;
        let mut result = parse(input, &ProviderContext::default()).unwrap();
        resolve_resource_refs(&mut result).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("my-bucket".to_string()))
        );
    }

    #[test]
    fn partial_application_let_binding_dropped_from_variables() {
        // After #2230 a `let` binding holding a partial application
        // is an evaluator-only artifact: it lives on `EvalValue`
        // during parsing so a later pipe / call can finish it, but
        // it never reaches `ParsedFile.variables`. Parsing succeeds;
        // the binding simply does not appear in the user-facing
        // variable map.
        let input = r#"
            let f = map(".subnet_id")
        "#;
        let result = parse(input, &ProviderContext::default())
            .expect("partial application in let binding should parse");
        assert!(result.variables.get("f").is_none());
    }

    #[test]
    fn partial_application_join_with_pipe() {
        // `["a", "b"] |> join(",")` desugars to join(",", ["a","b"]) which is a full call.
        // At parse time it stays as FunctionCall; resolution evaluates it.
        let input = r#"
            let bucket = aws.s3_bucket {
                name = ["a", "b"] |> join(",")
            }
        "#;
        let mut result = parse(input, &ProviderContext::default()).unwrap();
        resolve_resource_refs(&mut result).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("a,b".to_string()))
        );
    }

    #[test]
    fn partial_application_closure_direct_call() {
        // `let f = join(","); let x = f(["a", "b"])` should work
        let input = r#"
            let f = join(",")
            let x = f(["a", "b"])
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.variables.get("x"),
            Some(&Value::String("a,b".to_string()))
        );
    }

    #[test]
    fn partial_application_chained_pipes() {
        // `["a", "b"] |> join(",") |> upper()` — resolved via resource refs
        let input = r#"
            let bucket = aws.s3_bucket {
                name = ["a", "b"] |> join(",") |> upper()
            }
        "#;
        let mut result = parse(input, &ProviderContext::default()).unwrap();
        resolve_resource_refs(&mut result).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("A,B".to_string()))
        );
    }

    #[test]
    fn partial_application_closure_pipe() {
        // `let f = join(","); let x = ["a", "b"] |> f()` should work
        let input = r#"
            let f = join(",")
            let x = ["a", "b"] |> f()
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.variables.get("x"),
            Some(&Value::String("a,b".to_string()))
        );
    }

    #[test]
    fn partial_application_too_many_args_errors() {
        // Calling a closure with too many args should error
        let input = r#"
            let f = join(",")
            let x = f(["a", "b"], "extra")
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
    }

    #[test]
    fn partial_application_replace() {
        // `replace` has arity 3, partial application with 2 args
        let input = r#"
            let dash_to_underscore = replace("-", "_")
            let x = "hello-world" |> dash_to_underscore()
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.variables.get("x"),
            Some(&Value::String("hello_world".to_string()))
        );
    }

    #[test]
    fn partial_application_in_resource_attribute() {
        // Partial application in a resource attribute via pipe
        let input = r#"
            let bucket = aws.s3_bucket {
                name = ["my", "bucket"] |> join("-")
            }
        "#;
        let mut parsed = parse(input, &ProviderContext::default()).unwrap();
        resolve_resource_refs(&mut parsed).unwrap();
        assert_eq!(
            parsed.resources[0].get_attr("name"),
            Some(&Value::String("my-bucket".to_string()))
        );
    }

    #[test]
    fn partial_application_closure_in_resource_attribute() {
        // Closure variable used in resource attribute via pipe
        let input = r#"
            let dash_join = join("-")
            let bucket = aws.s3_bucket {
                name = ["my", "bucket"] |> dash_join()
            }
        "#;
        let mut parsed = parse(input, &ProviderContext::default()).unwrap();
        resolve_resource_refs(&mut parsed).unwrap();
        assert_eq!(
            parsed.resources[0].get_attr("name"),
            Some(&Value::String("my-bucket".to_string()))
        );
    }

    #[test]
    fn partial_application_closure_direct_call_in_resource_attribute() {
        // Closure variable used in resource attribute via direct call
        let input = r#"
            let dash_join = join("-")
            let bucket = aws.s3_bucket {
                name = dash_join(["my", "bucket"])
            }
        "#;
        let mut parsed = parse(input, &ProviderContext::default()).unwrap();
        resolve_resource_refs(&mut parsed).unwrap();
        assert_eq!(
            parsed.resources[0].get_attr("name"),
            Some(&Value::String("my-bucket".to_string()))
        );
    }

    #[test]
    fn forward_reference_parsed_as_resource_ref() {
        // Issue #866: Forward references should be resolved as ResourceRef,
        // not silently left as a plain string.
        let input = r#"
            let subnet = awscc.ec2.Subnet {
                vpc_id     = vpc.vpc_id
                cidr_block = "10.0.1.0/24"
            }

            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 2);

        let subnet = &result.resources[0];
        // Forward reference vpc.vpc_id should be a ResourceRef, not a plain String
        assert_eq!(
            subnet.get_attr("vpc_id"),
            Some(&Value::resource_ref(
                "vpc".to_string(),
                "vpc_id".to_string(),
                vec![]
            )),
            "Forward reference should be parsed as ResourceRef, got: {:?}",
            subnet.get_attr("vpc_id")
        );
    }

    #[test]
    fn forward_reference_resolve_works() {
        // Issue #866: parse_and_resolve should work with forward references
        let input = r#"
            let subnet = awscc.ec2.Subnet {
                vpc_id     = vpc.vpc_id
                cidr_block = "10.0.1.0/24"
            }

            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        // parse_and_resolve should not error on forward references
        let result = parse_and_resolve(input);
        assert!(
            result.is_ok(),
            "parse_and_resolve should succeed with forward references, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn forward_reference_unused_binding_detection() {
        // Forward-referenced bindings should be detected as used
        let input = r#"
            let subnet = awscc.ec2.Subnet {
                vpc_id     = vpc.vpc_id
                cidr_block = "10.0.1.0/24"
            }

            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let unused = crate::validation::check_unused_bindings(&parsed);
        // vpc is referenced by subnet, so should NOT be unused
        assert!(
            !unused.contains(&"vpc".to_string()),
            "vpc should not be unused, but check_unused_bindings returned: {:?}",
            unused
        );
    }

    #[test]
    fn forward_reference_in_nested_value() {
        // Forward references inside list/map values should also be resolved
        let input = r#"
            let subnet = awscc.ec2.Subnet {
                vpc_id     = vpc.vpc_id
                cidr_block = "10.0.1.0/24"
                tags = [{ vpc_ref = vpc.vpc_id }]
            }

            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[0];
        // Check nested reference in list > map
        if let Some(Value::List(items)) = subnet.get_attr("tags") {
            if let Some(Value::Map(map)) = items.first() {
                assert_eq!(
                    map.get("vpc_ref"),
                    Some(&Value::resource_ref(
                        "vpc".to_string(),
                        "vpc_id".to_string(),
                        vec![]
                    )),
                    "Nested forward reference should be resolved"
                );
            } else {
                panic!("Expected map in tags list");
            }
        } else {
            panic!("Expected tags to be a list");
        }
    }

    #[test]
    fn forward_reference_chained_three_parts() {
        // Issue #1259: Chained forward references like "later.attr.nested" should
        // be resolved to ResourceRef with field_path, not left as a plain string.
        let input = r#"
            let subnet = awscc.ec2.Subnet {
                vpc_id     = vpc.encryption_specification.status
                cidr_block = "10.0.1.0/24"
            }

            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[0];
        assert_eq!(
            subnet.get_attr("vpc_id"),
            Some(&Value::resource_ref(
                "vpc".to_string(),
                "encryption_specification".to_string(),
                vec!["status".to_string()]
            )),
            "Chained forward reference should be parsed as ResourceRef with field_path"
        );
    }

    #[test]
    fn forward_reference_chained_four_parts() {
        // Issue #1259: Deep chained forward references like "later.attr.deep.nested"
        // should be resolved to ResourceRef with multiple field_path entries.
        let input = r#"
            let subnet = awscc.ec2.Subnet {
                vpc_id     = vpc.config.deep.nested
                cidr_block = "10.0.1.0/24"
            }

            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[0];
        assert_eq!(
            subnet.get_attr("vpc_id"),
            Some(&Value::resource_ref(
                "vpc".to_string(),
                "config".to_string(),
                vec!["deep".to_string(), "nested".to_string()]
            )),
            "Deep chained forward reference should have multiple field_path entries"
        );
    }

    #[test]
    fn duplicate_let_binding_resource_produces_error() {
        // Issue #915: Duplicate let bindings should produce an error,
        // not silently overwrite the first binding.
        let input = r#"
            let rt = awscc.ec2.RouteTable {
                vpc_id = "vpc-123"
            }

            let rt = awscc.ec2.RouteTable {
                vpc_id = "vpc-456"
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_err(),
            "Duplicate let binding 'rt' should produce an error, but parsing succeeded: {:?}",
            result.unwrap()
        );
        let err = result.unwrap_err();
        match &err {
            ParseError::DuplicateBinding { name, line } => {
                assert_eq!(name, "rt");
                assert_eq!(
                    *line, 6,
                    "Duplicate binding should report the line of the second 'let rt', got line {line}"
                );
            }
            _ => panic!("Expected DuplicateBinding error, got: {err}"),
        }
        let err_str = err.to_string();
        assert!(
            err_str.contains("Duplicate") && err_str.contains("rt"),
            "Error should mention duplicate binding 'rt', got: {err_str}"
        );
    }

    #[test]
    fn duplicate_let_binding_variable_produces_error() {
        // Issue #915: Duplicate variable bindings should also produce an error.
        let input = r#"
            let region = aws.Region.ap_northeast_1
            let region = aws.Region.us_east_1
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_err(),
            "Duplicate let binding 'region' should produce an error, but parsing succeeded: {:?}",
            result.unwrap()
        );
        let err = result.unwrap_err();
        match &err {
            ParseError::DuplicateBinding { name, line } => {
                assert_eq!(name, "region");
                assert_eq!(
                    *line, 3,
                    "Duplicate binding should report the line of the second 'let region', got line {line}"
                );
            }
            _ => panic!("Expected DuplicateBinding error, got: {err}"),
        }
        let err_str = err.to_string();
        assert!(
            err_str.contains("Duplicate") && err_str.contains("region"),
            "Error should mention duplicate binding 'region', got: {err_str}"
        );
    }

    #[test]
    fn distinct_let_bindings_are_accepted() {
        // Sanity check: different binding names should work fine
        let input = r#"
            let rt1 = awscc.ec2.RouteTable {
                vpc_id = "vpc-123"
            }

            let rt2 = awscc.ec2.RouteTable {
                vpc_id = "vpc-456"
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_ok(),
            "Distinct let bindings should parse successfully, got: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().resources.len(), 2);
    }

    #[test]
    fn parse_error_has_internal_error_variant() {
        // Verify the InternalError variant exists and formats correctly
        let err = ParseError::InternalError {
            expected: "identifier".to_string(),
            context: "provider block".to_string(),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("expected identifier in provider block"),
            "InternalError should format with expected and context, got: {msg}"
        );
    }

    #[test]
    fn parse_slash_slash_comment_standalone() {
        let input = r#"
            // This is a C-style comment
            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_slash_slash_comment_inline() {
        let input = r#"
            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"  // inline comment
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_mixed_comment_styles() {
        let input = r#"
            # shell-style comment
            // C-style comment
            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"  // inline C-style
                tags = { Name = "main" }    # inline shell-style
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_block_comment_single_line() {
        let input = r#"
            /* single line block comment */
            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_block_comment_multi_line() {
        let input = r#"
            /*
              Multi-line block comment.
              All content is ignored by the parser.
            */
            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_block_comment_nested() {
        let input = r#"
            /* outer
              /* inner comment */
              still commented out
            */
            provider aws {
                region = aws.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "aws");
    }

    #[test]
    fn parse_block_comment_inline() {
        let input = r#"
            let vpc = awscc.ec2.Vpc {
                cidr_block = /* inline block comment */ "10.0.0.0/16"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_block_comment_with_all_comment_styles() {
        let input = r#"
            # shell-style comment
            // C-style comment
            /* block comment */
            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"  // inline C-style
                tags = { Name = "main" }    # inline shell-style
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
    }

    #[test]
    fn parse_provider_block_with_default_tags() {
        let input = r#"
            provider awscc {
                region = awscc.Region.ap_northeast_1
                default_tags = {
                    Environment = "production"
                    Team        = "platform"
                    ManagedBy   = "carina"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert_eq!(result.providers[0].name, "awscc");
        // default_tags should be extracted from attributes
        assert!(!result.providers[0].attributes.contains_key("default_tags"));
        assert_eq!(result.providers[0].default_tags.len(), 3);
        assert_eq!(
            result.providers[0].default_tags.get("Environment"),
            Some(&Value::String("production".to_string()))
        );
        assert_eq!(
            result.providers[0].default_tags.get("Team"),
            Some(&Value::String("platform".to_string()))
        );
        assert_eq!(
            result.providers[0].default_tags.get("ManagedBy"),
            Some(&Value::String("carina".to_string()))
        );
    }

    #[test]
    fn parse_provider_block_without_default_tags() {
        let input = r#"
            provider awscc {
                region = awscc.Region.ap_northeast_1
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.providers.len(), 1);
        assert!(result.providers[0].default_tags.is_empty());
    }

    #[test]
    fn parse_provider_block_with_source_and_version() {
        let input = r#"
            provider mock {
                source = "github.com/carina-rs/carina-provider-mock"
                version = "0.1.0"
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(parsed.providers.len(), 1);

        let provider = &parsed.providers[0];
        assert_eq!(provider.name, "mock");
        assert_eq!(
            provider.source.as_deref(),
            Some("github.com/carina-rs/carina-provider-mock")
        );
        assert_eq!(provider.version.as_ref().unwrap().raw, "0.1.0");
        // source and version should NOT be in attributes
        assert!(!provider.attributes.contains_key("source"));
        assert!(!provider.attributes.contains_key("version"));
    }

    #[test]
    fn parse_provider_block_without_source() {
        let input = r#"
            provider awscc {
                region = awscc.Region.ap_northeast_1
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let provider = &parsed.providers[0];
        assert!(provider.source.is_none());
        assert!(provider.version.is_none());
    }

    #[test]
    fn parse_provider_block_with_version_constraint() {
        let input = r#"
            provider mock {
                source = "github.com/carina-rs/carina-provider-mock"
                version = "~0.5.0"
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let provider = &parsed.providers[0];
        let vc = provider.version.as_ref().unwrap();
        assert_eq!(vc.raw, "~0.5.0");
        assert!(vc.matches("0.5.3").unwrap());
        assert!(!vc.matches("0.6.0").unwrap());
    }

    #[test]
    fn parse_provider_block_with_invalid_version_constraint() {
        let input = r#"
            provider mock {
                source = "github.com/carina-rs/carina-provider-mock"
                version = "not-valid"
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
    }

    #[test]
    fn parse_provider_block_with_revision() {
        let input = r#"
            provider mock {
                source = "github.com/carina-rs/carina-provider-mock"
                revision = "feature-branch"
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(parsed.providers.len(), 1);

        let provider = &parsed.providers[0];
        assert_eq!(provider.name, "mock");
        assert_eq!(
            provider.source.as_deref(),
            Some("github.com/carina-rs/carina-provider-mock")
        );
        assert_eq!(provider.revision.as_deref(), Some("feature-branch"));
        assert!(provider.version.is_none());
        assert!(!provider.attributes.contains_key("revision"));
    }

    #[test]
    fn parse_provider_block_with_revision_sha() {
        let input = r#"
            provider mock {
                source = "github.com/carina-rs/carina-provider-mock"
                revision = "abc123def456"
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let provider = &parsed.providers[0];
        assert_eq!(provider.revision.as_deref(), Some("abc123def456"));
    }

    #[test]
    fn parse_provider_block_version_and_revision_mutually_exclusive() {
        let input = r#"
            provider mock {
                source = "github.com/carina-rs/carina-provider-mock"
                version = "0.1.0"
                revision = "main"
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("mutually exclusive"),
            "Error should mention mutual exclusivity, got: {err}"
        );
    }

    #[test]
    fn resolve_resource_refs_with_argument_parameters() {
        let input = r#"
            arguments {
                cidr_block: String
                subnet_cidr: String
                az: String
            }

            let vpc = awscc.ec2.Vpc {
                cidr_block = cidr_block
            }

            let subnet = awscc.ec2.Subnet {
                vpc_id = vpc.vpc_id
                cidr_block = subnet_cidr
                availability_zone = az
            }

            attributes {
                vpc_id: awscc.ec2.Vpc = vpc.vpc_id
            }
        "#;

        // parse_and_resolve should succeed without "Undefined variable" errors
        let result = parse_and_resolve(input);
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());

        let parsed = result.unwrap();
        assert_eq!(parsed.resources.len(), 2); // allow: direct — fixture test inspection
        assert_eq!(parsed.arguments.len(), 3);
    }

    #[test]
    fn parse_let_binding_module_call() {
        let input = r#"
            let web_tier = use { source = "./modules/web_tier" }

            let web = web_tier {
                vpc = "vpc-123"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.module_calls.len(), 1);

        let call = &result.module_calls[0];
        assert_eq!(call.module_name, "web_tier");
        assert_eq!(call.binding_name, Some("web".to_string()));
        assert_eq!(
            call.arguments.get("vpc"),
            Some(&Value::String("vpc-123".to_string()))
        );
    }

    #[test]
    fn parse_module_call_binding_enables_resource_ref() {
        // After `let web = web_tier { ... }`, `web.security_group` should
        // resolve as ResourceRef.
        let input = r#"
            let web_tier = use { source = "./modules/web_tier" }

            let web = web_tier {
                vpc = "vpc-123"
            }

            let sg = awscc.ec2.SecurityGroup {
                group_description = "test"
                group_name = web.security_group
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let sg = &result.resources[0];
        assert_eq!(
            sg.get_attr("group_name"),
            Some(&Value::resource_ref(
                "web".to_string(),
                "security_group".to_string(),
                vec![]
            ))
        );
    }

    #[test]
    fn parse_string_interpolation_simple() {
        let input = r#"
            let env = "prod"
            let vpc = aws.ec2.Vpc {
                name = "vpc-${env}"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.get_attr("name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("vpc-".to_string()),
                InterpolationPart::Expr(Value::String("prod".to_string())),
            ]))
        );
    }

    #[test]
    fn parse_string_interpolation_multiple_exprs() {
        let input = r#"
            let env = "prod"
            let region = "us-east-1"
            let vpc = aws.ec2.Vpc {
                name = "vpc-${env}-${region}"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.get_attr("name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("vpc-".to_string()),
                InterpolationPart::Expr(Value::String("prod".to_string())),
                InterpolationPart::Literal("-".to_string()),
                InterpolationPart::Expr(Value::String("us-east-1".to_string())),
            ]))
        );
    }

    #[test]
    fn parse_string_interpolation_with_resource_ref() {
        let input = r#"
            let vpc = aws.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }
            let subnet = aws.ec2.Subnet {
                name = "subnet-${vpc.vpc_id}"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[1];
        assert_eq!(
            subnet.get_attr("name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("subnet-".to_string()),
                InterpolationPart::Expr(Value::resource_ref(
                    "vpc".to_string(),
                    "vpc_id".to_string(),
                    vec![]
                )),
            ]))
        );
    }

    #[test]
    fn parse_string_no_interpolation() {
        // Strings without ${} should remain as plain Value::String
        let input = r#"
            let vpc = aws.ec2.Vpc {
                name = "my-vpc"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.get_attr("name"),
            Some(&Value::String("my-vpc".to_string()))
        );
    }

    #[test]
    fn parse_string_dollar_without_brace() {
        // A $ not followed by { should be literal
        let input = r#"
            let vpc = aws.ec2.Vpc {
                name = "price$100"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.get_attr("name"),
            Some(&Value::String("price$100".to_string()))
        );
    }

    #[test]
    fn parse_string_escaped_interpolation() {
        // \${ should be literal ${
        let input = r#"
            let vpc = aws.ec2.Vpc {
                name = "literal\${expr}"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.get_attr("name"),
            Some(&Value::String("literal${expr}".to_string()))
        );
    }

    #[test]
    fn parse_string_interpolation_with_bool() {
        let input = r#"
            let vpc = aws.ec2.Vpc {
                name = "enabled-${true}"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.get_attr("name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("enabled-".to_string()),
                InterpolationPart::Expr(Value::Bool(true)),
            ]))
        );
    }

    #[test]
    fn parse_string_interpolation_with_number() {
        let input = r#"
            let vpc = aws.ec2.Vpc {
                name = "port-${8080}"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.get_attr("name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("port-".to_string()),
                InterpolationPart::Expr(Value::Int(8080)),
            ]))
        );
    }

    #[test]
    fn parse_string_interpolation_only_expr() {
        // String with only interpolation, no literal parts
        let input = r#"
            let name = "prod"
            let vpc = aws.ec2.Vpc {
                tag = "${name}"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.get_attr("tag"),
            Some(&Value::Interpolation(vec![InterpolationPart::Expr(
                Value::String("prod".to_string())
            ),]))
        );
    }

    #[test]
    fn parse_local_let_binding_in_resource_block() {
        let input = r#"
            let subnet = awscc.ec2.Subnet {
                let name = "my-subnet"
                cidr_block = "10.0.1.0/24"
                tag_name = name
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[0];

        // Local let binding should NOT appear in attributes
        assert!(!subnet.attributes.contains_key("name"));

        // The local binding value should be resolved in subsequent attributes
        assert_eq!(
            subnet.get_attr("tag_name"),
            Some(&Value::String("my-subnet".to_string()))
        );
        assert_eq!(
            subnet.get_attr("cidr_block"),
            Some(&Value::String("10.0.1.0/24".to_string()))
        );
    }

    #[test]
    fn parse_local_let_binding_with_interpolation() {
        let input = r#"
            let env = "prod"
            let subnet = awscc.ec2.Subnet {
                let name = "app-${env}"
                cidr_block = "10.0.1.0/24"
                tag_name = name
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[0];

        // Local binding should resolve outer scope variable in interpolation
        assert_eq!(
            subnet.get_attr("tag_name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Literal("app-".to_string()),
                InterpolationPart::Expr(Value::String("prod".to_string())),
            ]))
        );
    }

    #[test]
    fn parse_local_let_binding_chain() {
        let input = r#"
            let subnet = awscc.ec2.Subnet {
                let prefix = "app"
                let name = "${prefix}-subnet"
                cidr_block = "10.0.1.0/24"
                tag_name = name
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[0];

        // Chained local bindings should resolve correctly
        assert_eq!(
            subnet.get_attr("tag_name"),
            Some(&Value::Interpolation(vec![
                InterpolationPart::Expr(Value::String("app".to_string())),
                InterpolationPart::Literal("-subnet".to_string()),
            ]))
        );

        // Local bindings should NOT appear in attributes
        assert!(!subnet.attributes.contains_key("prefix"));
        assert!(!subnet.attributes.contains_key("name"));
    }

    #[test]
    fn parse_local_let_binding_with_function_call() {
        let input = r#"
            let subnet = awscc.ec2.Subnet {
                let name = "my-subnet"
                cidr_block = "10.0.1.0/24"
                tag_name = upper(name)
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[0];

        // Local binding used inside function call
        assert_eq!(
            subnet.get_attr("tag_name"),
            Some(&Value::FunctionCall {
                name: "upper".to_string(),
                args: vec![Value::String("my-subnet".to_string())],
            })
        );
    }

    #[test]
    fn parse_local_let_binding_in_anonymous_resource() {
        let input = r#"
            awscc.ec2.Subnet {
                let name = "my-subnet"
                cidr_block = "10.0.1.0/24"
                tag_name = name
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[0];

        // Local let binding should work in anonymous resources too
        assert!(!subnet.attributes.contains_key("name"));
        assert_eq!(
            subnet.get_attr("tag_name"),
            Some(&Value::String("my-subnet".to_string()))
        );
    }

    #[test]
    fn parse_local_let_binding_in_nested_block() {
        let input = r#"
            let subnet = awscc.ec2.Subnet {
                let env = "prod"
                cidr_block = "10.0.1.0/24"
                tags {
                    Name = env
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[0];

        // Local binding should be visible in nested blocks
        if let Some(Value::List(tags_list)) = subnet.get_attr("tags") {
            if let Some(Value::Map(tags)) = tags_list.first() {
                assert_eq!(tags.get("Name"), Some(&Value::String("prod".to_string())));
            } else {
                panic!("Expected Map in tags list");
            }
        } else {
            panic!("Expected tags attribute as List");
        }
    }

    #[test]
    fn parse_for_expression_over_list() {
        let input = r#"
            let subnets = for az in ["ap-northeast-1a", "ap-northeast-1c"] {
                awscc.ec2.Subnet {
                    availability_zone = az
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        // for expression expands to individual resources at parse time
        assert_eq!(result.resources.len(), 2);

        // Resources should be addressed as subnets[0] and subnets[1]
        assert_eq!(result.resources[0].id.name_str(), "subnets[0]");
        assert_eq!(result.resources[1].id.name_str(), "subnets[1]");

        // Each resource should have the loop variable substituted
        assert_eq!(
            result.resources[0].get_attr("availability_zone"),
            Some(&Value::String("ap-northeast-1a".to_string()))
        );
        assert_eq!(
            result.resources[1].get_attr("availability_zone"),
            Some(&Value::String("ap-northeast-1c".to_string()))
        );
    }

    #[test]
    fn parse_for_expression_with_index() {
        let input = r#"
            let subnets = for (i, az) in ["ap-northeast-1a", "ap-northeast-1c"] {
                awscc.ec2.Subnet {
                    availability_zone = az
                    cidr_block = cidr_subnet("10.0.0.0/16", 8, i)
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 2);

        assert_eq!(result.resources[0].id.name_str(), "subnets[0]");
        assert_eq!(result.resources[1].id.name_str(), "subnets[1]");

        // Check index variable is substituted
        if let Some(Value::FunctionCall { args, .. }) = result.resources[0].get_attr("cidr_block") {
            assert_eq!(args[2], Value::Int(0));
        } else {
            panic!("Expected FunctionCall for cidr_block");
        }

        if let Some(Value::FunctionCall { args, .. }) = result.resources[1].get_attr("cidr_block") {
            assert_eq!(args[2], Value::Int(1));
        } else {
            panic!("Expected FunctionCall for cidr_block");
        }
    }

    #[test]
    fn parse_for_expression_over_map() {
        let input = r#"
            let cidrs = {
                prod    = "10.0.0.0/16"
                staging = "10.1.0.0/16"
            }

            let networks = for name, cidr in cidrs {
                awscc.ec2.Vpc {
                    cidr_block = cidr
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 2);

        // Map iteration produces map-keyed addresses
        let names: Vec<&str> = result
            .resources
            .iter()
            .map(|r| r.id.name.as_str())
            .collect();
        assert!(names.contains(&r#"networks["prod"]"#));
        assert!(names.contains(&r#"networks["staging"]"#));
    }

    #[test]
    fn parse_for_expression_with_local_binding() {
        let input = r#"
            let subnets = for (i, az) in ["ap-northeast-1a", "ap-northeast-1c"] {
                let cidr = cidr_subnet("10.0.0.0/16", 8, i)
                awscc.ec2.Subnet {
                    cidr_block = cidr
                    availability_zone = az
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 2);

        // Local binding should be resolved within each iteration
        if let Some(Value::FunctionCall { name, args }) = result.resources[0].get_attr("cidr_block")
        {
            assert_eq!(name, "cidr_subnet");
            assert_eq!(args[2], Value::Int(0));
        } else {
            panic!("Expected FunctionCall for cidr_block");
        }
    }

    #[test]
    fn parse_for_expression_with_module_call() {
        let input = r#"
            let web = use { source = "modules/web" }

            let envs = {
                prod    = "10.0.0.0/16"
                staging = "10.1.0.0/16"
            }

            let webs = for name, cidr in envs {
                web { vpc_cidr = cidr }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();

        // for expression with module call should produce module calls, not resources
        assert_eq!(result.module_calls.len(), 2);

        // Module calls should have binding names like webs["prod"] and webs["staging"]
        let binding_names: Vec<&str> = result
            .module_calls
            .iter()
            .map(|c| c.binding_name.as_deref().unwrap())
            .collect();
        assert!(binding_names.contains(&r#"webs["prod"]"#));
        assert!(binding_names.contains(&r#"webs["staging"]"#));

        // Each module call should have the loop variable substituted in arguments
        for call in &result.module_calls {
            assert_eq!(call.module_name, "web");
            assert!(call.arguments.contains_key("vpc_cidr"));
        }

        // Verify the argument values are the substituted loop values
        let prod_call = result
            .module_calls
            .iter()
            .find(|c| c.binding_name.as_deref() == Some(r#"webs["prod"]"#))
            .unwrap();
        assert_eq!(
            prod_call.arguments.get("vpc_cidr"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );

        let staging_call = result
            .module_calls
            .iter()
            .find(|c| c.binding_name.as_deref() == Some(r#"webs["staging"]"#))
            .unwrap();
        assert_eq!(
            staging_call.arguments.get("vpc_cidr"),
            Some(&Value::String("10.1.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_for_expression_with_module_call_over_list() {
        let input = r#"
            let web = use { source = "modules/web" }

            let webs = for cidr in ["10.0.0.0/16", "10.1.0.0/16"] {
                web { vpc_cidr = cidr }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();

        // for expression with module call over list
        assert_eq!(result.module_calls.len(), 2);
        assert_eq!(result.resources.len(), 0);

        assert_eq!(
            result.module_calls[0].binding_name.as_deref(),
            Some("webs[0]")
        );
        assert_eq!(
            result.module_calls[1].binding_name.as_deref(),
            Some("webs[1]")
        );

        assert_eq!(
            result.module_calls[0].arguments.get("vpc_cidr"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
        assert_eq!(
            result.module_calls[1].arguments.get("vpc_cidr"),
            Some(&Value::String("10.1.0.0/16".to_string()))
        );
    }

    #[test]
    fn test_chained_field_access_two_levels() {
        // a.b.c should parse as ResourceRef with binding_name="a", attribute_name="b", field_path=["c"]
        let input = r#"
            let vpc = awscc.ec2.Vpc {
                name = "test-vpc"
            }

            awscc.ec2.Subnet {
                name = "test-subnet"
                vpc_id = vpc.network.vpc_id
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[1];
        let vpc_id = subnet.get_attr("vpc_id").expect("vpc_id attribute");
        match vpc_id {
            Value::ResourceRef { path } => {
                let binding_name = path.binding();
                let attribute_name = path.attribute();
                let field_path = path.field_path();
                assert_eq!(binding_name, "vpc");
                assert_eq!(attribute_name, "network");
                assert_eq!(field_path, vec!["vpc_id"]);
            }
            other => panic!("Expected ResourceRef with field_path, got {:?}", other),
        }
    }

    #[test]
    fn test_chained_field_access_three_levels() {
        // a.b.c.d should parse as ResourceRef with binding_name="a", attribute_name="b", field_path=["c", "d"]
        let input = r#"
            let web = awscc.ec2.Vpc {
                name = "test"
            }

            awscc.ec2.Subnet {
                name = "test-subnet"
                vpc_id = web.output.network.vpc_id
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = &result.resources[1];
        let vpc_id = subnet.get_attr("vpc_id").expect("vpc_id attribute");
        match vpc_id {
            Value::ResourceRef { path } => {
                let binding_name = path.binding();
                let attribute_name = path.attribute();
                let field_path = path.field_path();
                assert_eq!(binding_name, "web");
                assert_eq!(attribute_name, "output");
                assert_eq!(field_path, vec!["network", "vpc_id"]);
            }
            other => panic!("Expected ResourceRef with field_path, got {:?}", other),
        }
    }

    #[test]
    fn parse_index_access_with_integer() {
        // subnets[0].subnet_id should parse as ResourceRef with binding_name="subnets[0]"
        let input = r#"
            let subnets = for az in ["ap-northeast-1a", "ap-northeast-1c"] {
                awscc.ec2.Subnet {
                    availability_zone = az
                }
            }

            awscc.ec2.RouteTable {
                name = "test"
                subnet_id = subnets[0].subnet_id
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let rt = result.resources.last().expect("route_table resource");
        let subnet_id = rt.get_attr("subnet_id").expect("subnet_id attribute");
        match subnet_id {
            Value::ResourceRef { path } => {
                let binding_name = path.binding();
                let attribute_name = path.attribute();
                let field_path = path.field_path();
                assert_eq!(binding_name, "subnets[0]");
                assert_eq!(attribute_name, "subnet_id");
                assert!(field_path.is_empty());
            }
            other => panic!("Expected ResourceRef, got {:?}", other),
        }
    }

    #[test]
    fn parse_index_access_with_string_key() {
        // networks["prod"].vpc_id should parse as ResourceRef with binding_name=r#networks["prod"]#
        let input = r#"
            let cidrs = {
                prod    = "10.0.0.0/16"
                staging = "10.1.0.0/16"
            }

            let networks = for name, cidr in cidrs {
                awscc.ec2.Vpc {
                    cidr_block = cidr
                }
            }

            awscc.ec2.Subnet {
                name = "test"
                vpc_id = networks["prod"].vpc_id
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = result.resources.last().expect("subnet resource");
        let vpc_id = subnet.get_attr("vpc_id").expect("vpc_id attribute");
        match vpc_id {
            Value::ResourceRef { path } => {
                let binding_name = path.binding();
                let attribute_name = path.attribute();
                let field_path = path.field_path();
                assert_eq!(binding_name, r#"networks["prod"]"#);
                assert_eq!(attribute_name, "vpc_id");
                assert!(field_path.is_empty());
            }
            other => panic!("Expected ResourceRef, got {:?}", other),
        }
    }

    #[test]
    fn parse_index_access_with_chained_fields() {
        // webs["prod"].security_group.id should parse with field_path
        let input = r#"
            let cidrs = {
                prod    = "10.0.0.0/16"
                staging = "10.1.0.0/16"
            }

            let webs = for name, cidr in cidrs {
                awscc.ec2.Vpc {
                    cidr_block = cidr
                }
            }

            awscc.ec2.Subnet {
                name = "test"
                sg_id = webs["prod"].security_group.id
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let subnet = result.resources.last().expect("subnet resource");
        let sg_id = subnet.get_attr("sg_id").expect("sg_id attribute");
        match sg_id {
            Value::ResourceRef { path } => {
                let binding_name = path.binding();
                let attribute_name = path.attribute();
                let field_path = path.field_path();
                assert_eq!(binding_name, r#"webs["prod"]"#);
                assert_eq!(attribute_name, "security_group");
                assert_eq!(field_path, vec!["id"]);
            }
            other => panic!("Expected ResourceRef with field_path, got {:?}", other),
        }
    }

    #[test]
    fn parse_import_block() {
        let input = r#"
            import {
                to = awscc.ec2.Vpc "main-vpc"
                id = "vpc-0abc123def456"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.state_blocks.len(), 1);
        match &result.state_blocks[0] {
            StateBlock::Import { to, id } => {
                assert_eq!(to.provider, "awscc");
                assert_eq!(to.resource_type, "ec2.Vpc");
                assert_eq!(to.name_str(), "main-vpc");
                assert_eq!(id, "vpc-0abc123def456");
            }
            other => panic!("Expected Import, got {:?}", other),
        }
    }

    #[test]
    fn parse_removed_block() {
        let input = r#"
            removed {
                from = awscc.ec2.Vpc "legacy-vpc"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.state_blocks.len(), 1);
        match &result.state_blocks[0] {
            StateBlock::Removed { from } => {
                assert_eq!(from.provider, "awscc");
                assert_eq!(from.resource_type, "ec2.Vpc");
                assert_eq!(from.name_str(), "legacy-vpc");
            }
            other => panic!("Expected Removed, got {:?}", other),
        }
    }

    #[test]
    fn parse_moved_block() {
        let input = r#"
            moved {
                from = awscc.ec2.Subnet "old-name"
                to   = awscc.ec2.Subnet "new-name"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.state_blocks.len(), 1);
        match &result.state_blocks[0] {
            StateBlock::Moved { from, to } => {
                assert_eq!(from.provider, "awscc");
                assert_eq!(from.resource_type, "ec2.Subnet");
                assert_eq!(from.name_str(), "old-name");
                assert_eq!(to.provider, "awscc");
                assert_eq!(to.resource_type, "ec2.Subnet");
                assert_eq!(to.name_str(), "new-name");
            }
            other => panic!("Expected Moved, got {:?}", other),
        }
    }

    #[test]
    fn parse_for_expression_with_keys_function_call() {
        let input = r#"
            let tags = {
                Name = "web"
                Env  = "prod"
            }

            let resources = for key in keys(tags) {
                awscc.ec2.Subnet {
                    name = key
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        // keys({Name = "web", Env = "prod"}) should evaluate to ["Env", "Name"] (sorted)
        assert_eq!(result.resources.len(), 2);
        assert_eq!(result.resources[0].id.name_str(), "resources[0]");
        assert_eq!(result.resources[1].id.name_str(), "resources[1]");
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("Env".to_string()))
        );
        assert_eq!(
            result.resources[1].get_attr("name"),
            Some(&Value::String("Name".to_string()))
        );
    }

    #[test]
    fn parse_for_expression_with_values_function_call() {
        let input = r#"
            let cidrs = {
                prod    = "10.0.0.0/16"
                staging = "10.1.0.0/16"
            }

            let networks = for cidr in values(cidrs) {
                awscc.ec2.Vpc {
                    cidr_block = cidr
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        // values() returns values sorted by key: prod, staging
        assert_eq!(result.resources.len(), 2);
        assert_eq!(
            result.resources[0].get_attr("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
        assert_eq!(
            result.resources[1].get_attr("cidr_block"),
            Some(&Value::String("10.1.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_for_expression_with_concat_function_call() {
        let input = r#"
            let networks = for cidr in concat(["10.0.0.0/16"], ["10.1.0.0/16"]) {
                awscc.ec2.Vpc {
                    cidr_block = cidr
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 2);
        // concat(items, base_list) => base_list ++ items
        // So concat(["10.0.0.0/16"], ["10.1.0.0/16"]) => ["10.1.0.0/16", "10.0.0.0/16"]
        assert_eq!(
            result.resources[0].get_attr("cidr_block"),
            Some(&Value::String("10.1.0.0/16".to_string()))
        );
        assert_eq!(
            result.resources[1].get_attr("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_for_expression_with_runtime_function_call_errors() {
        // Function call with runtime-dependent args (ResourceRef) should error
        let input = r#"
            let vpc = awscc.ec2.Vpc {
                name = "test"
            }

            let subnets = for key in keys(vpc.tags) {
                awscc.ec2.Subnet {
                    name = key
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("runtime"),
            "Expected error about runtime dependency, got: {}",
            err
        );
    }

    // ── if/else expression tests ──

    #[test]
    fn parse_if_true_condition_includes_resource() {
        let input = r#"
            let alarm = if true {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(result.resources[0].id.name_str(), "alarm");
        assert_eq!(
            result.resources[0].get_attr("alarm_name"),
            Some(&Value::String("cpu-high".to_string()))
        );
    }

    #[test]
    fn parse_if_false_condition_no_resource() {
        let input = r#"
            let alarm = if false {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 0);
    }

    #[test]
    fn parse_if_else_true_uses_if_branch() {
        let input = r#"
            let vpc = if true {
                awscc.ec2.Vpc {
                    cidr_block = "10.0.0.0/16"
                }
            } else {
                awscc.ec2.Vpc {
                    cidr_block = "172.16.0.0/16"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_if_else_false_uses_else_branch() {
        let input = r#"
            let vpc = if false {
                awscc.ec2.Vpc {
                    cidr_block = "10.0.0.0/16"
                }
            } else {
                awscc.ec2.Vpc {
                    cidr_block = "172.16.0.0/16"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("cidr_block"),
            Some(&Value::String("172.16.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_if_else_value_expression() {
        let input = r#"
            let instance_type = if true {
                "m5.xlarge"
            } else {
                "t3.micro"
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 0);
        // The binding should be set to the value from the true branch
        // We verify by using the variable in a resource
        let input2 = r#"
            let instance_type = if true {
                "m5.xlarge"
            } else {
                "t3.micro"
            }

            awscc.ec2.Instance {
                instance_type = instance_type
            }
        "#;

        let result2 = parse(input2, &ProviderContext::default()).unwrap();
        assert_eq!(
            result2.resources[0].get_attr("instance_type"),
            Some(&Value::String("m5.xlarge".to_string()))
        );
    }

    #[test]
    fn parse_if_else_value_expression_false_branch() {
        let input = r#"
            let instance_type = if false {
                "m5.xlarge"
            } else {
                "t3.micro"
            }

            awscc.ec2.Instance {
                instance_type = instance_type
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("instance_type"),
            Some(&Value::String("t3.micro".to_string()))
        );
    }

    #[test]
    fn parse_if_with_variable_condition() {
        let input = r#"
            let enable_monitoring = true

            let alarm = if enable_monitoring {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
    }

    #[test]
    fn parse_if_non_bool_condition_errors() {
        let input = r#"
            let alarm = if "not_a_bool" {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Bool"),
            "Expected error about Bool condition, got: {}",
            err
        );
    }

    #[test]
    fn parse_if_resource_ref_condition_errors() {
        let input = r#"
            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }

            let alarm = if vpc.enabled {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("runtime") || err.contains("statically"),
            "Expected error about runtime dependency, got: {}",
            err
        );
    }

    #[test]
    fn parse_if_with_module_call() {
        let input = r#"
            let web = use { source = "modules/web" }

            let monitoring = if true {
                web { vpc_id = "vpc-123" }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.module_calls.len(), 1);
        assert_eq!(result.module_calls[0].module_name, "web");
    }

    #[test]
    fn parse_if_false_with_module_call() {
        let input = r#"
            let web = use { source = "modules/web" }

            let monitoring = if false {
                web { vpc_id = "vpc-123" }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.module_calls.len(), 0);
    }

    #[test]
    fn parse_if_with_local_binding() {
        let input = r#"
            let alarm = if true {
                let name = "cpu-high"
                awscc.cloudwatch.alarm {
                    alarm_name = name
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("alarm_name"),
            Some(&Value::String("cpu-high".to_string()))
        );
    }

    #[test]
    fn parse_if_else_value_expr_in_attribute_true() {
        let input = r#"
            let is_production = true

            awscc.ec2.Vpc {
                cidr_block = if is_production { "10.0.0.0/16" } else { "172.16.0.0/16" }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_if_else_value_expr_in_attribute_false() {
        let input = r#"
            let is_production = false

            awscc.ec2.Vpc {
                cidr_block = if is_production { "10.0.0.0/16" } else { "172.16.0.0/16" }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("cidr_block"),
            Some(&Value::String("172.16.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_if_value_expr_no_else_true() {
        // When condition is true and no else, the value is used
        let input = r#"
            awscc.ec2.Vpc {
                cidr_block = if true { "10.0.0.0/16" }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
    }

    #[test]
    fn parse_if_value_expr_no_else_false_errors() {
        // When condition is false and no else, it's an error in value position
        let input = r#"
            awscc.ec2.Vpc {
                cidr_block = if false { "10.0.0.0/16" }
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("else"),
            "Expected error about missing else clause, got: {}",
            err
        );
    }

    #[test]
    fn parse_top_level_for_expression() {
        let input = r#"
            for az in ["ap-northeast-1a", "ap-northeast-1c"] {
                awscc.ec2.Subnet {
                    availability_zone = az
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 2);

        // Each resource should have the loop variable substituted
        assert_eq!(
            result.resources[0].get_attr("availability_zone"),
            Some(&Value::String("ap-northeast-1a".to_string()))
        );
        assert_eq!(
            result.resources[1].get_attr("availability_zone"),
            Some(&Value::String("ap-northeast-1c".to_string()))
        );
    }

    #[test]
    fn parse_top_level_if_expression() {
        let input = r#"
            let enabled = true
            if enabled {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("alarm_name"),
            Some(&Value::String("cpu-high".to_string()))
        );
    }

    #[test]
    fn parse_top_level_multiple_for_no_collision() {
        let input = r#"
            for az in ["a", "b"] {
                awscc.ec2.Subnet {
                    availability_zone = az
                }
            }
            for name in ["web", "api"] {
                awscc.ec2.SecurityGroup {
                    group_name = name
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 4);

        // First for gets _for0, second gets _for1 - no collisions
        let names: Vec<&str> = result
            .resources
            .iter()
            .map(|r| r.id.name.as_str())
            .collect();
        assert_eq!(names[0], "_for0[0]");
        assert_eq!(names[1], "_for0[1]");
        assert_eq!(names[2], "_for1[0]");
        assert_eq!(names[3], "_for1[1]");
    }

    #[test]
    fn parse_top_level_for_uses_iterable_name_as_binding() {
        let input = r#"
            let azs = ["ap-northeast-1a", "ap-northeast-1c"]
            for az in azs {
                awscc.ec2.Subnet {
                    availability_zone = az
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 2);

        let names: Vec<&str> = result
            .resources
            .iter()
            .map(|r| r.id.name.as_str())
            .collect();
        assert_eq!(names[0], "_azs[0]");
        assert_eq!(names[1], "_azs[1]");
    }

    #[test]
    fn parse_top_level_for_uses_last_segment_of_dotted_iterable() {
        let input = r#"
            let orgs = upstream_state {
                source = "../orgs"
            }
            for acct in orgs.accounts {
                awscc.sso.Assignment {
                    target_id = acct
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        // Deferred (upstream_state not resolved), so no concrete resources
        // but the deferred_for_expressions should use _accounts
        assert_eq!(result.deferred_for_expressions[0].binding_name, "_accounts");
    }

    #[test]
    fn parse_top_level_for_literal_list_uses_counter_fallback() {
        let input = r#"
            for az in ["a", "b"] {
                awscc.ec2.Subnet {
                    availability_zone = az
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let names: Vec<&str> = result
            .resources
            .iter()
            .map(|r| r.id.name.as_str())
            .collect();
        assert_eq!(names[0], "_for0[0]");
        assert_eq!(names[1], "_for0[1]");
    }

    #[test]
    fn parse_top_level_if_false_no_resources() {
        let input = r#"
            let enabled = false
            if enabled {
                awscc.cloudwatch.alarm {
                    alarm_name = "cpu-high"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 0);
    }

    #[test]
    fn parse_arguments_block_form_description_only() {
        let input = r#"
            arguments {
                vpc: awscc.ec2.Vpc {
                    description = "The VPC to deploy into"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments.len(), 1);
        assert_eq!(result.arguments[0].name, "vpc");
        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("awscc", "ec2.Vpc"))
        );
        assert!(result.arguments[0].default.is_none());
        assert_eq!(
            result.arguments[0].description.as_deref(),
            Some("The VPC to deploy into")
        );
    }

    #[test]
    fn parse_arguments_block_form_description_and_default() {
        let input = r#"
            arguments {
                port: Int {
                    description = "Web server port"
                    default     = 8080
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments.len(), 1);
        assert_eq!(result.arguments[0].name, "port");
        assert_eq!(result.arguments[0].type_expr, TypeExpr::Int);
        assert_eq!(result.arguments[0].default, Some(Value::Int(8080)));
        assert_eq!(
            result.arguments[0].description.as_deref(),
            Some("Web server port")
        );
    }

    #[test]
    fn parse_arguments_mixed_simple_and_block_form() {
        let input = r#"
            arguments {
                enable_https: Bool = true

                vpc: awscc.ec2.Vpc {
                    description = "The VPC to deploy into"
                }

                port: Int {
                    description = "Web server port"
                    default     = 8080
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments.len(), 3);

        // Simple form (unchanged)
        assert_eq!(result.arguments[0].name, "enable_https");
        assert_eq!(result.arguments[0].type_expr, TypeExpr::Bool);
        assert_eq!(result.arguments[0].default, Some(Value::Bool(true)));
        assert!(result.arguments[0].description.is_none());

        // Block form with description only
        assert_eq!(result.arguments[1].name, "vpc");
        assert_eq!(
            result.arguments[1].type_expr,
            TypeExpr::Ref(ResourceTypePath::new("awscc", "ec2.Vpc"))
        );
        assert!(result.arguments[1].default.is_none());
        assert_eq!(
            result.arguments[1].description.as_deref(),
            Some("The VPC to deploy into")
        );

        // Block form with description and default
        assert_eq!(result.arguments[2].name, "port");
        assert_eq!(result.arguments[2].type_expr, TypeExpr::Int);
        assert_eq!(result.arguments[2].default, Some(Value::Int(8080)));
        assert_eq!(
            result.arguments[2].description.as_deref(),
            Some("Web server port")
        );
    }

    #[test]
    fn parse_arguments_simple_form_has_no_description() {
        let input = r#"
            arguments {
                vpc_id: String
                port: Int = 8080
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments.len(), 2);
        assert!(result.arguments[0].description.is_none());
        assert!(result.arguments[1].description.is_none());
    }

    #[test]
    fn parse_accepts_pascal_case_primitives() {
        let input = r#"
            arguments {
                a: String
                b: Int
                c: Bool
                d: Float
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
        assert_eq!(result.arguments[1].type_expr, TypeExpr::Int);
        assert_eq!(result.arguments[2].type_expr, TypeExpr::Bool);
        assert_eq!(result.arguments[3].type_expr, TypeExpr::Float);
    }

    #[test]
    fn parse_still_accepts_lowercase_primitives_during_transition() {
        let input = r#"
            arguments {
                a: String
                b: Int
                c: Bool
                d: Float
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
        assert_eq!(result.arguments[1].type_expr, TypeExpr::Int);
        assert_eq!(result.arguments[2].type_expr, TypeExpr::Bool);
        assert_eq!(result.arguments[3].type_expr, TypeExpr::Float);
    }

    #[test]
    fn parse_accepts_pascal_case_custom_types() {
        let input = r#"
            arguments {
                id: AwsAccountId
                cidr: Ipv4Cidr
                bucket_arn: Arn
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::Simple("aws_account_id".to_string())
        );
        assert_eq!(
            result.arguments[1].type_expr,
            TypeExpr::Simple("ipv4_cidr".to_string())
        );
        assert_eq!(
            result.arguments[2].type_expr,
            TypeExpr::Simple("arn".to_string())
        );
    }

    #[test]
    fn parse_three_segment_resource_path_is_ref() {
        let input = r#"
            arguments {
                vpc: aws.ec2.Vpc
                bucket: aws.s3.Bucket
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        match &result.arguments[0].type_expr {
            TypeExpr::Ref(path) => {
                assert_eq!(path.provider, "aws");
                assert_eq!(path.resource_type, "ec2.Vpc");
            }
            other => panic!("expected Ref, got {other:?}"),
        }
        match &result.arguments[1].type_expr {
            TypeExpr::Ref(path) => {
                assert_eq!(path.provider, "aws");
                assert_eq!(path.resource_type, "s3.Bucket");
            }
            other => panic!("expected Ref, got {other:?}"),
        }
    }

    #[test]
    fn parse_four_segment_path_with_pascal_tail_is_schema_type() {
        let input = r#"
            arguments {
                vpc_id: awscc.ec2.VpcId
            }
        "#;
        let mut ctx = ProviderContext::default();
        ctx.register_schema_type("awscc", "ec2", "VpcId");
        let result = parse(input, &ctx).unwrap();
        assert!(matches!(
            result.arguments[0].type_expr,
            TypeExpr::SchemaType { .. }
        ));
    }

    #[test]
    fn type_expr_ref_display_roundtrips_three_segment_path() {
        let ty = TypeExpr::Ref(ResourceTypePath::new("aws", "ec2.Vpc"));
        assert_eq!(ty.to_string(), "aws.ec2.Vpc");

        let input = format!(r#"arguments {{ v: {} }}"#, ty);
        let parsed = parse(&input, &ProviderContext::default()).unwrap();
        assert_eq!(parsed.arguments[0].type_expr, ty);
    }

    #[test]
    fn parser_rejects_lowercase_primitive_after_phase_c() {
        // Intentionally uses the old snake_case spelling to verify Phase C
        // rejection, so the type annotation below must NOT be mechanically
        // rewritten to PascalCase.
        let input = "arguments { a: string }";
        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown type 'string'") && msg.contains("'String'"),
            "expected rejection with hint pointing at 'String', got: {msg}"
        );
    }

    #[test]
    fn parser_rejects_snake_case_custom_type_after_phase_c() {
        let input = "arguments { a: aws_account_id }";
        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown type 'aws_account_id'") && msg.contains("'AwsAccountId'"),
            "expected rejection with hint pointing at 'AwsAccountId', got: {msg}"
        );
    }

    #[test]
    fn parser_does_not_warn_on_new_spelling() {
        let input = r#"
            arguments {
                a: String
                b: AwsAccountId
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.message.contains("deprecated type spelling")),
            "should not warn on new spellings, got {:?}",
            result.warnings
        );
    }

    #[test]
    fn parse_arguments_block_form_default_only() {
        let input = r#"
            arguments {
                port: Int {
                    default = 8080
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments.len(), 1);
        assert_eq!(result.arguments[0].name, "port");
        assert_eq!(result.arguments[0].default, Some(Value::Int(8080)));
        assert!(result.arguments[0].description.is_none());
    }

    #[test]
    fn parse_arguments_block_form_empty_block() {
        let input = r#"
            arguments {
                port: Int {}
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments.len(), 1);
        assert_eq!(result.arguments[0].name, "port");
        assert!(result.arguments[0].default.is_none());
        assert!(result.arguments[0].description.is_none());
    }

    #[test]
    fn parse_arguments_block_form_string_default_not_confused_with_description() {
        let input = r#"
            arguments {
                name: String {
                    description = "Name of the resource"
                    default     = "my-resource"
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments.len(), 1);
        assert_eq!(result.arguments[0].name, "name");
        assert_eq!(result.arguments[0].type_expr, TypeExpr::String);
        assert_eq!(
            result.arguments[0].description.as_deref(),
            Some("Name of the resource")
        );
        assert_eq!(
            result.arguments[0].default,
            Some(Value::String("my-resource".to_string()))
        );
    }

    #[test]
    fn parse_arguments_block_form_validation_block() {
        let input = r#"
            arguments {
                port: Int {
                    description = "Web server port"
                    default     = 8080
                    validation {
                        condition   = port >= 1 && port <= 65535
                        error_message = "Port must be between 1 and 65535"
                    }
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments.len(), 1);
        let arg = &result.arguments[0];
        assert_eq!(arg.name, "port");
        assert_eq!(arg.type_expr, TypeExpr::Int);
        assert_eq!(arg.default, Some(Value::Int(8080)));
        assert_eq!(arg.description.as_deref(), Some("Web server port"));
        assert_eq!(arg.validations.len(), 1);
        assert_eq!(
            arg.validations[0].error_message.as_deref(),
            Some("Port must be between 1 and 65535")
        );

        // Verify the validate expression structure:
        // port >= 1 && port <= 65535
        match &arg.validations[0].condition {
            ValidateExpr::And(left, right) => {
                match left.as_ref() {
                    ValidateExpr::Compare { lhs, op, rhs } => {
                        assert_eq!(*lhs, Box::new(ValidateExpr::Var("port".to_string())));
                        assert_eq!(*op, CompareOp::Gte);
                        assert_eq!(*rhs, Box::new(ValidateExpr::Int(1)));
                    }
                    other => panic!("Expected Compare, got {:?}", other),
                }
                match right.as_ref() {
                    ValidateExpr::Compare { lhs, op, rhs } => {
                        assert_eq!(*lhs, Box::new(ValidateExpr::Var("port".to_string())));
                        assert_eq!(*op, CompareOp::Lte);
                        assert_eq!(*rhs, Box::new(ValidateExpr::Int(65535)));
                    }
                    other => panic!("Expected Compare, got {:?}", other),
                }
            }
            other => panic!("Expected And, got {:?}", other),
        }
    }

    #[test]
    fn parse_arguments_block_form_validate_no_description() {
        let input = r#"
            arguments {
                count: Int {
                    validation {
                        condition = count > 0
                    }
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments.len(), 1);
        let arg = &result.arguments[0];
        assert_eq!(arg.validations.len(), 1);
        assert!(arg.validations[0].error_message.is_none());
        assert!(arg.description.is_none());
        assert!(arg.default.is_none());
    }

    #[test]
    fn parse_arguments_block_form_validate_with_not() {
        let input = r#"
            arguments {
                enabled: Bool {
                    validation {
                        condition = !enabled == false
                    }
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments[0].validations.len(), 1);
    }

    #[test]
    fn parse_arguments_block_form_validate_with_or() {
        let input = r#"
            arguments {
                port: Int {
                    validation {
                        condition   = port == 80 || port == 443
                        error_message = "Port must be 80 or 443"
                    }
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        match &result.arguments[0].validations[0].condition {
            ValidateExpr::Or(_, _) => {}
            other => panic!("Expected Or, got {:?}", other),
        }
    }

    #[test]
    fn parse_arguments_block_form_validate_with_len() {
        let input = r#"
            arguments {
                name: String {
                    validation {
                        condition   = len(name) >= 1 && len(name) <= 64
                        error_message = "Name must be between 1 and 64 characters"
                    }
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments[0].validations.len(), 1);
        assert_eq!(
            result.arguments[0].validations[0].error_message.as_deref(),
            Some("Name must be between 1 and 64 characters")
        );
    }

    #[test]
    fn parse_arguments_block_form_multiple_validation_blocks() {
        let input = r#"
            arguments {
                port: Int {
                    validation {
                        condition   = port >= 1
                        error_message = "Port must be positive"
                    }
                    validation {
                        condition   = port <= 65535
                        error_message = "Port must be at most 65535"
                    }
                }
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments[0].validations.len(), 2);
        assert_eq!(
            result.arguments[0].validations[0].error_message.as_deref(),
            Some("Port must be positive")
        );
        assert_eq!(
            result.arguments[0].validations[1].error_message.as_deref(),
            Some("Port must be at most 65535")
        );
    }

    #[test]
    fn env_missing_var_produces_error_at_parse_time() {
        // Use a var name that is extremely unlikely to be set
        let input = r#"
            provider aws {
                region = aws.Region.ap_northeast_1
            }

            aws.s3.Bucket {
                name = env("CARINA_TEST_NONEXISTENT_VAR_12345")
            }
        "#;

        let result = parse_and_resolve(input);
        assert!(
            result.is_err(),
            "Expected error for missing env var, got: {:?}",
            result
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("CARINA_TEST_NONEXISTENT_VAR_12345"),
            "Error should mention the missing env var name, got: {}",
            err_msg
        );
    }

    #[test]
    fn join_with_resolved_args_still_works() {
        let input = r#"
            provider aws {
                region = aws.Region.ap_northeast_1
            }

            aws.s3.Bucket {
                name = join("-", ["a", "b", "c"])
            }
        "#;

        let result = parse_and_resolve(input).unwrap();
        let resource = &result.resources[0];
        assert_eq!(
            resource.get_attr("name"),
            Some(&Value::String("a-b-c".to_string())),
        );
    }

    // --- User-defined function tests ---

    #[test]
    fn user_fn_simple_call() {
        let input = r#"
            fn greet(name) {
                join(" ", ["hello", name])
            }

            let vpc = aws.s3_bucket {
                name = greet("world")
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("hello world".to_string())),
        );
    }

    #[test]
    fn user_fn_with_default_param() {
        let input = r#"
            fn tag(env, suffix = "default") {
                join("-", [env, suffix])
            }

            let a = aws.s3_bucket {
                name = tag("prod")
            }

            let b = aws.s3_bucket {
                name = tag("prod", "web")
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("prod-default".to_string())),
        );
        assert_eq!(
            result.resources[1].get_attr("name"),
            Some(&Value::String("prod-web".to_string())),
        );
    }

    #[test]
    fn user_fn_with_local_let() {
        let input = r#"
            fn subnet_name(env, az) {
                let prefix = join("-", [env, "subnet"])
                join("-", [prefix, az])
            }

            let vpc = aws.s3_bucket {
                name = subnet_name("prod", "a")
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("prod-subnet-a".to_string())),
        );
    }

    #[test]
    fn user_fn_calling_builtin() {
        let input = r#"
            fn upper_name(name) {
                upper(name)
            }

            let vpc = aws.s3_bucket {
                name = upper_name("hello")
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("HELLO".to_string())),
        );
    }

    #[test]
    fn user_fn_calling_another_fn() {
        let input = r#"
            fn prefix(env) {
                join("-", [env, "app"])
            }

            fn full_name(env, service) {
                join("-", [prefix(env), service])
            }

            let vpc = aws.s3_bucket {
                name = full_name("prod", "web")
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("prod-app-web".to_string())),
        );
    }

    #[test]
    fn user_fn_recursive_call_errors() {
        let input = r#"
            fn recurse(x) {
                recurse(x)
            }

            let vpc = aws.s3_bucket {
                name = recurse("hello")
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Recursive function call"),
            "Expected recursive function error, got: {err}"
        );
    }

    #[test]
    fn user_fn_missing_required_arg_errors() {
        let input = r#"
            fn greet(name, title) {
                join(" ", [title, name])
            }

            let vpc = aws.s3_bucket {
                name = greet("world")
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expects at least 2"),
            "Expected missing arg error, got: {err}"
        );
    }

    #[test]
    fn user_fn_too_many_args_errors() {
        let input = r#"
            fn greet(name) {
                join(" ", ["hello", name])
            }

            let vpc = aws.s3_bucket {
                name = greet("world", "extra")
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expects at most 1"),
            "Expected too many args error, got: {err}"
        );
    }

    #[test]
    fn user_fn_shadows_builtin_errors() {
        let input = r#"
            fn join(sep, items) {
                sep
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("shadows a built-in function"),
            "Expected shadow error, got: {err}"
        );
    }

    #[test]
    fn user_fn_duplicate_definition_errors() {
        let input = r#"
            fn greet(name) {
                name
            }

            fn greet(x) {
                x
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("duplicate function definition"),
            "Expected duplicate error, got: {err}"
        );
    }

    #[test]
    fn user_fn_stored_in_parsed_file() {
        let input = r#"
            fn greet(name) {
                join(" ", ["hello", name])
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert!(result.user_functions.contains_key("greet"));
        let func = &result.user_functions["greet"];
        assert_eq!(func.name, "greet");
        assert_eq!(func.params.len(), 1);
        assert_eq!(func.params[0].name, "name");
    }

    #[test]
    fn user_fn_no_params() {
        let input = r#"
            fn hello() {
                "hello"
            }

            let vpc = aws.s3_bucket {
                name = hello()
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("hello".to_string())),
        );
    }

    #[test]
    fn user_fn_indirect_recursion_errors() {
        let input = r#"
            fn foo(x) {
                bar(x)
            }

            fn bar(x) {
                foo(x)
            }

            let vpc = aws.s3_bucket {
                name = foo("hello")
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Recursive function call"),
            "Expected recursive function error, got: {err}"
        );
    }

    #[test]
    fn user_fn_required_param_after_optional_errors() {
        let input = r#"
            fn bad(a = "x", b) {
                join("-", [a, b])
            }
        "#;

        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("required parameter") && err.contains("cannot follow optional"),
            "Expected param ordering error, got: {err}"
        );
    }

    #[test]
    fn user_fn_with_pipe_operator() {
        let input = r#"
            fn wrap(prefix, val) {
                join("-", [prefix, val])
            }

            let vpc = aws.s3_bucket {
                name = "world" |> wrap("hello")
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("hello-world".to_string())),
        );
    }

    #[test]
    fn user_fn_with_string_interpolation() {
        let input = r#"
            fn greet(name) {
                join(" ", ["hello", name])
            }

            let vpc = aws.s3_bucket {
                name = "${greet("world")}-suffix"
            }
        "#;

        // At parse time, fn is evaluated but interpolation is not fully resolved
        let result = parse(input, &ProviderContext::default()).unwrap();
        let name = result.resources[0].get_attr("name").unwrap();
        match name {
            Value::Interpolation(parts) => {
                // The greet() call is evaluated to "hello world"
                assert_eq!(parts.len(), 2);
                assert_eq!(
                    parts[0],
                    InterpolationPart::Expr(Value::String("hello world".to_string()))
                );
                assert_eq!(parts[1], InterpolationPart::Literal("-suffix".to_string()));
            }
            _ => panic!("Expected Interpolation, got: {:?}", name),
        }
    }

    #[test]
    fn user_fn_typed_param_string() {
        let input = r#"
            fn greet(name: String) {
                join(" ", ["hello", name])
            }

            let vpc = aws.s3_bucket {
                name = greet("world")
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("hello world".to_string())),
        );
    }

    #[test]
    fn user_fn_typed_param_type_mismatch() {
        let input = r#"
            fn greet(name: String) {
                name
            }

            let vpc = aws.s3_bucket {
                name = greet(42)
            }
        "#;

        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("expects type 'String'"),
            "Expected type mismatch error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_typed_param_int() {
        let input = r#"
            fn double(x: Int) {
                x
            }

            let vpc = aws.s3_bucket {
                name = double("not_int")
            }
        "#;

        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("expects type 'Int'"),
            "Expected type mismatch error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_typed_param_with_default() {
        let input = r#"
            fn tag(env: String, suffix: String = "default") {
                join("-", [env, suffix])
            }

            let a = aws.s3_bucket {
                name = tag("prod")
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("prod-default".to_string())),
        );
    }

    #[test]
    fn user_fn_mixed_typed_and_untyped() {
        let input = r#"
            fn tag(env, suffix: String) {
                join("-", [env, suffix])
            }

            let a = aws.s3_bucket {
                name = tag("prod", "web")
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.resources[0].get_attr("name"),
            Some(&Value::String("prod-web".to_string())),
        );
    }

    #[test]
    fn user_fn_typed_param_bool_mismatch() {
        let input = r#"
            fn check(flag: Bool) {
                flag
            }

            let vpc = aws.s3_bucket {
                name = check("not_bool")
            }
        "#;

        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("expects type 'Bool'"),
            "Expected type mismatch error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_param_type_stored_in_parsed_file() {
        let input = r#"
            fn greet(name: String, count: Int) {
                name
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let func = result.user_functions.get("greet").unwrap();
        assert_eq!(func.params[0].param_type, Some(TypeExpr::String));
        assert_eq!(func.params[1].param_type, Some(TypeExpr::Int));
    }

    #[test]
    fn user_fn_untyped_param_type_is_none() {
        let input = r#"
            fn greet(name) {
                name
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let func = result.user_functions.get("greet").unwrap();
        assert_eq!(func.params[0].param_type, None);
    }

    #[test]
    fn user_fn_return_type_string() {
        let input = r#"
            fn greet(name: String): String {
                name
            }

            let vpc = aws.s3_bucket {
                name = greet("hello")
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let func = result.user_functions.get("greet").unwrap();
        assert_eq!(func.return_type, Some(TypeExpr::String));
    }

    #[test]
    fn user_fn_return_type_none_when_omitted() {
        let input = r#"
            fn greet(name) {
                name
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let func = result.user_functions.get("greet").unwrap();
        assert_eq!(func.return_type, None);
    }

    #[test]
    fn user_fn_return_type_mismatch_value() {
        let input = r#"
            fn bad(): String {
                42
            }

            let vpc = aws.s3_bucket {
                name = bad()
            }
        "#;

        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("return type"),
            "Expected return type error, got: {msg}"
        );
    }

    #[test]
    fn parse_custom_schema_type_in_fn_param() {
        // Custom schema types like ipv4_cidr, ipv4_address, arn should be accepted as type annotations
        let input = r#"
            fn format_cidr(cidr_block: Ipv4Cidr) {
                cidr_block
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        let func = result.user_functions.get("format_cidr").unwrap();
        assert_eq!(func.params[0].name, "cidr_block");
        assert_eq!(
            func.params[0].param_type,
            Some(TypeExpr::Simple("ipv4_cidr".to_string()))
        );
    }

    #[test]
    fn parse_ipv4_address_type_in_fn_param() {
        let input = r#"
            fn f(addr: Ipv4Address) {
                addr
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        let func = result.user_functions.get("f").unwrap();
        assert_eq!(
            func.params[0].param_type,
            Some(TypeExpr::Simple("ipv4_address".to_string()))
        );
    }

    #[test]
    fn parse_arn_type_in_fn_param() {
        let input = r#"
            fn f(role: Arn) {
                role
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        let func = result.user_functions.get("f").unwrap();
        assert_eq!(
            func.params[0].param_type,
            Some(TypeExpr::Simple("arn".to_string()))
        );
    }

    #[test]
    fn parse_custom_type_in_list_generic() {
        let input = r#"
            fn f(cidrs: list(Ipv4Cidr)) {
                cidrs
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        let func = result.user_functions.get("f").unwrap();
        assert_eq!(
            func.params[0].param_type,
            Some(TypeExpr::List(Box::new(TypeExpr::Simple(
                "ipv4_cidr".to_string()
            ))))
        );
    }

    #[test]
    fn parse_custom_type_in_module_arguments() {
        let input = r#"
            arguments {
                vpc_cidr: Ipv4Cidr
                server_ip: Ipv4Address
            }

            awscc.ec2.Vpc {
                name       = "test"
                cidr_block = vpc_cidr
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.arguments[0].name, "vpc_cidr");
        assert_eq!(
            result.arguments[0].type_expr,
            TypeExpr::Simple("ipv4_cidr".to_string())
        );
        assert_eq!(result.arguments[1].name, "server_ip");
        assert_eq!(
            result.arguments[1].type_expr,
            TypeExpr::Simple("ipv4_address".to_string())
        );
    }

    #[test]
    fn parse_custom_type_in_attributes() {
        let input = r#"
            attributes {
                block: Ipv4Cidr = vpc.cidr_block
            }

            let vpc = awscc.ec2.Vpc {
                name       = "test"
                cidr_block = "10.0.0.0/16"
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.attribute_params[0].type_expr,
            Some(TypeExpr::Simple("ipv4_cidr".to_string()))
        );
    }

    #[test]
    fn type_expr_display_simple() {
        assert_eq!(
            TypeExpr::Simple("ipv4_cidr".to_string()).to_string(),
            "Ipv4Cidr"
        );
        assert_eq!(
            TypeExpr::Simple("ipv4_address".to_string()).to_string(),
            "Ipv4Address"
        );
        assert_eq!(TypeExpr::Simple("arn".to_string()).to_string(), "Arn");
    }

    #[test]
    fn type_expr_display_simple_is_pascal_case() {
        assert_eq!(
            TypeExpr::Simple("aws_account_id".to_string()).to_string(),
            "AwsAccountId"
        );
        assert_eq!(
            TypeExpr::Simple("ipv4_cidr".to_string()).to_string(),
            "Ipv4Cidr"
        );
        assert_eq!(TypeExpr::Simple("arn".to_string()).to_string(), "Arn");
    }

    // --- Issue #1285: Validate fn call arguments for custom types ---

    #[test]
    fn user_fn_custom_type_cidr_arg_valid() {
        let input = r#"
            fn f(x: Ipv4Cidr) { x }

            let b = aws.s3_bucket {
                name = f("10.0.0.0/16")
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
    }

    #[test]
    fn user_fn_custom_type_cidr_arg_invalid() {
        let input = r#"
            fn f(x: Ipv4Cidr) { x }

            let b = aws.s3_bucket {
                name = f("invalid")
            }
        "#;
        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("type 'ipv4_cidr' validation failed"),
            "Expected ipv4_cidr validation error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_custom_type_ipv4_address_arg_valid() {
        let input = r#"
            fn f(x: Ipv4Address) { x }

            let b = aws.s3_bucket {
                name = f("10.0.0.1")
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
    }

    #[test]
    fn user_fn_custom_type_ipv4_address_arg_invalid() {
        let input = r#"
            fn f(x: Ipv4Address) { x }

            let b = aws.s3_bucket {
                name = f("invalid")
            }
        "#;
        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("type 'ipv4_address' validation failed"),
            "Expected ipv4_address validation error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_custom_type_ipv6_cidr_arg_valid() {
        let input = r#"
            fn f(x: Ipv6Cidr) { x }

            let b = aws.s3_bucket {
                name = f("2001:db8::/32")
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
    }

    #[test]
    fn user_fn_custom_type_ipv6_cidr_arg_invalid() {
        let input = r#"
            fn f(x: Ipv6Cidr) { x }

            let b = aws.s3_bucket {
                name = f("invalid")
            }
        "#;
        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("type 'ipv6_cidr' validation failed"),
            "Expected ipv6_cidr validation error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_custom_type_ipv6_address_arg_valid() {
        let input = r#"
            fn f(x: Ipv6Address) { x }

            let b = aws.s3_bucket {
                name = f("2001:db8::1")
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
    }

    #[test]
    fn user_fn_custom_type_ipv6_address_arg_invalid() {
        let input = r#"
            fn f(x: Ipv6Address) { x }

            let b = aws.s3_bucket {
                name = f("invalid")
            }
        "#;
        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("type 'ipv6_address' validation failed"),
            "Expected ipv6_address validation error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_custom_type_arn_arg_accepts_string() {
        // arn format varies too much, just accept any string
        let input = r#"
            fn f(x: Arn) { x }

            let b = aws.s3_bucket {
                name = f("arn:aws:s3:::my-bucket")
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
    }

    #[test]
    fn user_fn_custom_type_arg_resource_ref_skipped() {
        // ResourceRef values should be accepted (resolved later)
        let input = r#"
            fn f(x: Ipv4Cidr) { x }

            let vpc = awscc.ec2.Vpc {
                cidr_block = "10.0.0.0/16"
            }

            let b = aws.s3_bucket {
                name = f(vpc.cidr_block)
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
    }

    // --- Issue #1284: Validate fn return type for custom types ---

    #[test]
    fn user_fn_custom_type_return_cidr_valid() {
        let input = r#"
            fn f(): Ipv4Cidr { "10.0.0.0/16" }

            let b = aws.s3_bucket {
                name = f()
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_ok(), "Expected OK, got: {:?}", result.err());
    }

    #[test]
    fn user_fn_custom_type_return_cidr_invalid() {
        let input = r#"
            fn f(): Ipv4Cidr { "invalid" }

            let b = aws.s3_bucket {
                name = f()
            }
        "#;
        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("return type 'ipv4_cidr' validation failed"),
            "Expected ipv4_cidr validation error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_custom_type_return_ipv4_address_invalid() {
        let input = r#"
            fn f(): Ipv4Address { "invalid" }

            let b = aws.s3_bucket {
                name = f()
            }
        "#;
        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("return type 'ipv4_address' validation failed"),
            "Expected ipv4_address validation error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_custom_type_return_ipv6_cidr_invalid() {
        let input = r#"
            fn f(): Ipv6Cidr { "invalid" }

            let b = aws.s3_bucket {
                name = f()
            }
        "#;
        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("return type 'ipv6_cidr' validation failed"),
            "Expected ipv6_cidr validation error, got: {msg}"
        );
    }

    #[test]
    fn user_fn_custom_type_return_ipv6_address_invalid() {
        let input = r#"
            fn f(): Ipv6Address { "invalid" }

            let b = aws.s3_bucket {
                name = f()
            }
        "#;
        let err = parse(input, &ProviderContext::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("return type 'ipv6_address' validation failed"),
            "Expected ipv6_address validation error, got: {msg}"
        );
    }

    // --- ProviderContext tests ---

    #[test]
    fn parse_decrypt_uses_config_decryptor() {
        use std::collections::HashMap;
        let config = ProviderContext {
            decryptor: Some(Box::new(|ciphertext, _key| {
                Ok(format!("decrypted:{ciphertext}"))
            })),
            validators: HashMap::new(),
            custom_type_validator: None,
            schema_types: Default::default(),
        };

        // decrypt() in resource attributes is resolved during resolve_resource_refs,
        // so we need to parse and then resolve with config.
        let input = r#"
            let my_bucket = aws.s3_bucket {
                name   = "test-bucket"
                secret = decrypt("AQICAHh")
            }
        "#;
        let mut parsed = parse(input, &config).unwrap();
        resolve_resource_refs_with_config(&mut parsed, &config).unwrap();
        assert_eq!(parsed.resources.len(), 1); // allow: direct — fixture test inspection
        let secret_val = parsed.resources[0].get_attr("secret").unwrap();
        assert_eq!(*secret_val, Value::String("decrypted:AQICAHh".to_string()));
    }

    #[test]
    fn parse_decrypt_without_decryptor_errors() {
        let config = ProviderContext::default();

        let input = r#"
            let my_bucket = aws.s3_bucket {
                name   = "test-bucket"
                secret = decrypt("AQICAHh")
            }
        "#;
        let mut parsed = parse(input, &config).unwrap();
        let result = resolve_resource_refs_with_config(&mut parsed, &config);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("requires a configured provider"),
            "Expected decryptor error, got: {msg}"
        );
    }

    #[test]
    fn parse_custom_validator_accepts_valid() {
        use std::collections::HashMap;
        // Test validate_custom_type directly with a type name that has no built-in
        // handler. Built-in types (cidr, ipv4_address, etc.) are matched first in
        // validate_custom_type, so custom validators only apply to other type names.
        let mut validators: HashMap<String, ValidatorFn> = HashMap::new();
        validators.insert(
            "custom_type".to_string(),
            Box::new(|s: &str| {
                if s.starts_with("valid-") {
                    Ok(())
                } else {
                    Err(format!("custom_type must start with 'valid-', got '{s}'"))
                }
            }),
        );
        let config = ProviderContext {
            decryptor: None,
            validators,
            custom_type_validator: None,
            schema_types: Default::default(),
        };

        let result = validate_custom_type(
            "custom_type",
            &Value::String("valid-data".to_string()),
            &config,
        );
        assert!(result.is_ok());

        // Unknown type with no custom validator should also pass (permissive)
        let result = validate_custom_type(
            "unknown_type",
            &Value::String("anything".to_string()),
            &config,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn parse_custom_validator_rejects_invalid() {
        use std::collections::HashMap;
        // Use a type name that the grammar accepts and has no built-in validator.
        // The "arn" type is accepted by the grammar as identifier. But it fails to parse.
        // Use "cidr" which is known to work in grammar. Register a custom stricter validator.
        // Actually, let's test validate_custom_type directly to avoid grammar issues.
        let mut validators: HashMap<String, ValidatorFn> = HashMap::new();
        validators.insert(
            "custom_type".to_string(),
            Box::new(|s: &str| {
                if s.starts_with("valid-") {
                    Ok(())
                } else {
                    Err(format!("custom_type must start with 'valid-', got '{s}'"))
                }
            }),
        );
        let config = ProviderContext {
            decryptor: None,
            validators,
            custom_type_validator: None,
            schema_types: Default::default(),
        };

        // Test validate_custom_type directly since the grammar may not accept
        // arbitrary type names. This verifies the custom validator is called.
        let valid_result = validate_custom_type(
            "custom_type",
            &Value::String("valid-data".to_string()),
            &config,
        );
        assert!(valid_result.is_ok());

        let invalid_result = validate_custom_type(
            "custom_type",
            &Value::String("invalid".to_string()),
            &config,
        );
        assert!(invalid_result.is_err());
        let msg = invalid_result.unwrap_err();
        assert!(
            msg.contains("custom_type must start with 'valid-'"),
            "Expected validation error, got: {msg}"
        );
    }

    #[test]
    fn pascal_to_snake_conversion() {
        assert_eq!(super::pascal_to_snake("VpcId"), "vpc_id");
        assert_eq!(super::pascal_to_snake("SubnetId"), "subnet_id");
        assert_eq!(
            super::pascal_to_snake("SecurityGroupId"),
            "security_group_id"
        );
        assert_eq!(super::pascal_to_snake("Arn"), "arn");
        assert_eq!(super::pascal_to_snake("IamRoleArn"), "iam_role_arn");
    }

    #[test]
    fn snake_to_pascal_conversion() {
        use super::snake_to_pascal;
        assert_eq!(snake_to_pascal("vpc_id"), "VpcId");
        assert_eq!(snake_to_pascal("aws_account_id"), "AwsAccountId");
        assert_eq!(snake_to_pascal("iam_policy_arn"), "IamPolicyArn");
        assert_eq!(snake_to_pascal("ipv4_cidr"), "Ipv4Cidr");
        assert_eq!(snake_to_pascal("arn"), "Arn");
        assert_eq!(snake_to_pascal("kms_key_arn"), "KmsKeyArn");
        for name in [
            "vpc_id",
            "aws_account_id",
            "iam_policy_arn",
            "ipv4_cidr",
            "arn",
        ] {
            assert_eq!(pascal_to_snake(&snake_to_pascal(name)), name);
        }
    }

    #[test]
    fn parse_schema_type_in_arguments() {
        let input = r#"
arguments {
  vpc_id: awscc.ec2.VpcId
}
"#;
        let mut ctx = ProviderContext::default();
        ctx.register_schema_type("awscc", "ec2", "VpcId");
        let parsed = parse(input, &ctx).unwrap();
        assert_eq!(parsed.arguments.len(), 1);
        let arg = &parsed.arguments[0];
        assert_eq!(arg.name, "vpc_id");
        match &arg.type_expr {
            TypeExpr::SchemaType {
                provider,
                path,
                type_name,
            } => {
                assert_eq!(provider, "awscc");
                assert_eq!(path, "ec2");
                assert_eq!(type_name, "VpcId");
            }
            other => panic!("Expected SchemaType, got {:?}", other),
        }
    }

    #[test]
    fn parse_schema_type_display() {
        let t = TypeExpr::SchemaType {
            provider: "awscc".to_string(),
            path: "ec2".to_string(),
            type_name: "VpcId".to_string(),
        };
        assert_eq!(t.to_string(), "awscc.ec2.VpcId");
    }

    #[test]
    fn parse_schema_type_list() {
        let input = r#"
arguments {
  subnet_ids: list(awscc.ec2.SubnetId)
}
"#;
        let mut ctx = ProviderContext::default();
        ctx.register_schema_type("awscc", "ec2", "SubnetId");
        let parsed = parse(input, &ctx).unwrap();
        assert_eq!(parsed.arguments.len(), 1);
        let arg = &parsed.arguments[0];
        match &arg.type_expr {
            TypeExpr::List(inner) => match inner.as_ref() {
                TypeExpr::SchemaType { type_name, .. } => {
                    assert_eq!(type_name, "SubnetId");
                }
                other => panic!("Expected SchemaType inside list, got {:?}", other),
            },
            other => panic!("Expected List, got {:?}", other),
        }
    }

    #[test]
    fn parse_let_discard_read_resource() {
        let input = r#"
            provider aws {
                region = aws.Region.ap_northeast_1
            }

            let _ = read aws.sts.caller_identity {}
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.resources.len(), 1);
        assert_eq!(result.resources[0].id.resource_type, "sts.caller_identity");
        assert_eq!(
            result.resources[0].kind,
            crate::resource::ResourceKind::DataSource
        );
    }

    #[test]
    fn parse_upstream_state_registers_binding() {
        // After parsing upstream_state, the binding should be registered so that
        // `network.vpc.vpc_id` is parsed as a ResourceRef.
        let input = r#"
            let network = upstream_state {
                source = "../network"
            }

            let web_sg = awscc.ec2.SecurityGroup {
                name = "web-sg"
                vpc_id = network.vpc.vpc_id
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.upstream_states.len(), 1);
        assert_eq!(result.resources.len(), 1);
        let vpc_id_attr = result.resources[0].get_attr("vpc_id").unwrap();
        match vpc_id_attr {
            Value::ResourceRef { path } => {
                assert_eq!(path.binding(), "network");
                assert_eq!(path.attribute(), "vpc");
                assert_eq!(path.field_path(), vec!["vpc_id"]);
            }
            other => panic!("Expected ResourceRef, got: {:?}", other),
        }
    }

    #[test]
    fn test_parse_require_statement() {
        let input = r#"
            arguments {
                enable_https: Bool = true
                has_cert: Bool = false
            }
            require !enable_https || has_cert, "cert is required when HTTPS is enabled"
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.requires.len(), 1);
        assert_eq!(
            result.requires[0].error_message,
            "cert is required when HTTPS is enabled"
        );
        // Verify the condition is an Or expression
        match &result.requires[0].condition {
            ValidateExpr::Or(_, _) => {}
            other => panic!("Expected Or expression, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_require_with_len_function() {
        let input = r#"
            arguments {
                subnet_ids: list(String)
            }
            require len(subnet_ids) >= 2, "ALB requires at least two subnets"
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.requires.len(), 1);
        assert_eq!(
            result.requires[0].error_message,
            "ALB requires at least two subnets"
        );
        match &result.requires[0].condition {
            ValidateExpr::Compare { lhs, op, rhs } => {
                assert!(
                    matches!(lhs.as_ref(), ValidateExpr::FunctionCall { name, .. } if name == "len")
                );
                assert_eq!(*op, CompareOp::Gte);
                assert_eq!(**rhs, ValidateExpr::Int(2));
            }
            other => panic!("Expected Compare expression, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_require_with_null() {
        let input = r#"
            arguments {
                cert_arn: String = "default"
            }
            require cert_arn != null, "cert_arn must not be null"
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.requires.len(), 1);
        match &result.requires[0].condition {
            ValidateExpr::Compare { lhs, op, rhs } => {
                assert!(matches!(lhs.as_ref(), ValidateExpr::Var(name) if name == "cert_arn"));
                assert_eq!(*op, CompareOp::Ne);
                assert_eq!(**rhs, ValidateExpr::Null);
            }
            other => panic!("Expected Compare expression, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_multiple_require_statements() {
        let input = r#"
            arguments {
                min_size: Int
                max_size: Int
                subnet_ids: list(String)
            }
            require min_size <= max_size, "min_size must be <= max_size"
            require len(subnet_ids) >= 2, "need at least two subnets"
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.requires.len(), 2);
        assert_eq!(
            result.requires[0].error_message,
            "min_size must be <= max_size"
        );
        assert_eq!(
            result.requires[1].error_message,
            "need at least two subnets"
        );
    }

    #[test]
    fn test_parse_require_with_and_operator() {
        let input = r#"
            arguments {
                port: Int = 80
            }
            require port >= 1 && port <= 65535, "port must be between 1 and 65535"
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.requires.len(), 1);
        match &result.requires[0].condition {
            ValidateExpr::And(_, _) => {}
            other => panic!("Expected And expression, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_require_null_prefixed_variable() {
        // Ensure variables with names starting with "null" (e.g., "nullable")
        // are not mis-parsed as null_literal
        let input = r#"
            arguments {
                nullable: Bool = true
            }
            require nullable, "must be true"
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(result.requires.len(), 1);
        match &result.requires[0].condition {
            ValidateExpr::Var(name) => {
                assert_eq!(name, "nullable");
            }
            other => panic!("Expected Var('nullable'), got {:?}", other),
        }
    }

    #[test]
    fn test_compose_operator_followed_by_pipe_consumes_closure() {
        // After #2230, the composed closure produced by `>>` lives on
        // `EvalValue` and is consumed by the later pipe. The
        // intermediate binding `f` is an evaluator artifact and is
        // dropped at the parse boundary; only the fully-reduced
        // `result` survives.
        let input = r#"
            let f = map(".id") >> join(",")
            let result = [{ id = "a" }, { id = "b" }] |> f()
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();

        assert_eq!(
            result.variables.get("result").unwrap(),
            &Value::String("a,b".to_string())
        );
        // `f` is a closure-only binding and does not appear in the
        // user-facing variable map.
        assert!(result.variables.get("f").is_none());
    }

    #[test]
    fn test_compose_operator_with_pipe() {
        // Compose then use via pipe
        let input = r#"
            let transform = map(".name") >> join(", ")
            let names = [{ name = "alice" }, { name = "bob" }] |> transform()
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.variables.get("names").unwrap(),
            &Value::String("alice, bob".to_string())
        );
    }

    #[test]
    fn test_compose_operator_two_step_chain() {
        // split(",") >> join("-") composed and applied
        let input = r#"
            let transform = split(",") >> join("-")
            let result = "a,b,c" |> transform()
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.variables.get("result").unwrap(),
            &Value::String("a-b-c".to_string())
        );
    }

    #[test]
    fn test_compose_operator_error_on_non_closure_lhs() {
        // "hello" >> join(",") should fail
        let input = r#"
            let f = "hello" >> join(",")
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("left side of >> must be a Closure"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_compose_operator_error_on_non_closure_rhs() {
        // join(",") >> "hello" should fail
        let input = r#"
            let f = join(",") >> "hello"
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("right side of >> must be a Closure"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn test_compose_operator_precedence_with_pipe() {
        // Compose used with pipe via variable
        let input = r#"
            let pipeline = map(".x") >> join("-")
            let data = [{ x = "1" }, { x = "2" }]
            let result = data |> pipeline()
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.variables.get("result").unwrap(),
            &Value::String("1-2".to_string())
        );
    }

    #[test]
    fn test_compose_three_functions() {
        // Three-way composition: parser must accept the chain and
        // (via #2230) keep the result confined to the evaluator-only
        // `EvalValue` layer. The binding is dropped from the
        // user-facing variable map; the test that the chain still
        // *applies* correctly is covered by
        // `test_compose_operator_followed_by_pipe_consumes_closure`.
        let input = r#"
            let transform = split(",") >> join("-") >> split("-")
        "#;
        let result =
            parse(input, &ProviderContext::default()).expect("three-way composition should parse");
        assert!(result.variables.get("transform").is_none());
    }

    #[test]
    fn parse_single_quoted_string_literal() {
        let input = r#"
            let vpc = aws.ec2.Vpc {
                name = 'my-vpc'
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.get_attr("name"),
            Some(&Value::String("my-vpc".to_string()))
        );
    }

    #[test]
    fn parse_single_quoted_string_no_interpolation() {
        // Single-quoted strings should NOT support interpolation — ${...} is literal
        let input = r#"
            let env = "prod"
            let vpc = aws.ec2.Vpc {
                name = 'vpc-${env}'
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let vpc = &result.resources[0];
        // Should be a plain string, not interpolated
        assert_eq!(
            vpc.get_attr("name"),
            Some(&Value::String("vpc-${env}".to_string()))
        );
    }

    #[test]
    fn parse_single_quoted_string_escape_sequences() {
        let input = r#"
            let vpc = aws.ec2.Vpc {
                name = 'it\'s a test'
            }
        "#;

        let result = parse(input, &ProviderContext::default()).unwrap();
        let vpc = &result.resources[0];
        assert_eq!(
            vpc.get_attr("name"),
            Some(&Value::String("it's a test".to_string()))
        );
    }

    #[test]
    fn test_compose_three_functions_execution() {
        // Three-way composition applied end-to-end:
        // split(",") >> join("-") >> split("-") — split, rejoin, then split again
        let input = r#"
            let transform = split(",") >> join("-") >> split("-")
            let result = "a,b,c" |> transform()
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.variables.get("result").unwrap(),
            &Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
                Value::String("c".to_string()),
            ])
        );
    }

    #[test]
    fn parse_heredoc_basic() {
        let input = r#"
            aws.iam.Role {
                name = "my-role"
                policy = <<EOT
{
  "Version": "2012-10-17"
}
EOT
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        let resource = &result.resources[0];
        assert_eq!(
            resource.get_attr("policy"),
            Some(&Value::String(
                "{\n  \"Version\": \"2012-10-17\"\n}".to_string()
            ))
        );
    }

    #[test]
    fn parse_heredoc_indented() {
        // <<- strips common leading whitespace
        let input = "aws.iam.Role {\n    name = \"my-role\"\n    policy = <<-EOT\n        line1\n        line2\n        line3\n    EOT\n}\n";
        let result = parse(input, &ProviderContext::default()).unwrap();
        let resource = &result.resources[0];
        assert_eq!(
            resource.get_attr("policy"),
            Some(&Value::String("line1\nline2\nline3".to_string()))
        );
    }

    #[test]
    fn parse_heredoc_empty() {
        let input = "aws.iam.Role {\n    name = \"my-role\"\n    policy = <<EOT\nEOT\n}\n";
        let result = parse(input, &ProviderContext::default()).unwrap();
        let resource = &result.resources[0];
        assert_eq!(
            resource.get_attr("policy"),
            Some(&Value::String("".to_string()))
        );
    }

    #[test]
    fn parse_heredoc_in_let_binding() {
        let input = r#"
            let doc = <<EOF
hello world
EOF
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(
            result.variables.get("doc"),
            Some(&Value::String("hello world".to_string()))
        );
    }

    #[test]
    fn quoted_string_as_map_key() {
        let input = r#"
            let m = {
                'token.actions.githubusercontent.com:aud' = 'sts.amazonaws.com'
                "aws:SourceIp" = '10.0.0.0/8'
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        if let Some(Value::Map(map)) = result.variables.get("m") {
            assert_eq!(
                map.get("token.actions.githubusercontent.com:aud"),
                Some(&Value::String("sts.amazonaws.com".to_string()))
            );
            assert_eq!(
                map.get("aws:SourceIp"),
                Some(&Value::String("10.0.0.0/8".to_string()))
            );
        } else {
            panic!("Expected map, got {:?}", result.variables.get("m"));
        }
    }

    #[test]
    fn quoted_string_as_attribute_key_in_block() {
        let input = r#"
            awscc.iam.role {
                name = 'test-role'
                assume_role_policy_document = {
                    version = '2012-10-17'
                    statement {
                        effect = 'Allow'
                        action = 'sts:AssumeRoleWithWebIdentity'
                        condition = {
                            string_equals = {
                                'token.actions.githubusercontent.com:aud' = 'sts.amazonaws.com'
                            }
                        }
                    }
                }
            }
        "#;
        let result = parse(input, &ProviderContext::default()).unwrap();
        let resource = &result.resources[0];
        // Navigate: assume_role_policy_document -> statement[0] -> condition -> string_equals
        let doc = resource.get_attr("assume_role_policy_document").unwrap();
        if let Value::Map(doc_map) = doc
            && let Some(Value::List(statements)) = doc_map.get("statement")
            && let Value::Map(stmt) = &statements[0]
            && let Some(Value::Map(condition)) = stmt.get("condition")
            && let Some(Value::Map(string_equals)) = condition.get("string_equals")
        {
            assert_eq!(
                string_equals.get("token.actions.githubusercontent.com:aud"),
                Some(&Value::String("sts.amazonaws.com".to_string()))
            );
        } else {
            panic!("Could not navigate to condition key");
        }
    }

    #[test]
    fn parse_exports_block_basic() {
        let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

exports {
  vpc_id = vpc.vpc_id
}
"#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(parsed.export_params.len(), 1);
        assert_eq!(parsed.export_params[0].name, "vpc_id");
        assert!(parsed.export_params[0].type_expr.is_none());
        assert!(parsed.export_params[0].value.is_some());
    }

    #[test]
    fn parse_exports_block_with_type() {
        let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

exports {
  vpc_id: String = vpc.vpc_id
  cidr: String = vpc.cidr_block
}
"#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(parsed.export_params.len(), 2);
        assert_eq!(parsed.export_params[0].name, "vpc_id");
        assert!(parsed.export_params[0].type_expr.is_some());
        assert_eq!(parsed.export_params[1].name, "cidr");
    }

    #[test]
    fn parse_exports_block_list_round_trips_through_formatter() {
        let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

exports {
  vpc_ids: list(String) = [
    vpc.vpc_id,
  ]
}
"#;

        let original = parse(input, &ProviderContext::default()).unwrap();
        let formatted =
            crate::formatter::format(input, &crate::formatter::FormatConfig::default()).unwrap();
        let reparsed = parse(&formatted, &ProviderContext::default()).unwrap();

        assert_eq!(
            formatted,
            r#"provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

exports {
  vpc_ids: list(String) = [vpc.vpc_id]
}
"#
        );
        assert_eq!(original.export_params, reparsed.export_params);
    }

    #[test]
    fn coalesce_operator_returns_default_for_unresolved_ref() {
        let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}

awscc.ec2.Subnet {
  cidr_block = vpc.missing_attr ?? '10.0.1.0/24'
}
"#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        // vpc.missing_attr is a ResourceRef (unresolved at parse time), so ?? returns default
        let subnet = parsed
            .resources
            .iter()
            .find(|r| r.id.resource_type == "ec2.Subnet")
            .unwrap();
        let cidr = subnet.get_attr("cidr_block");
        // At parse time, vpc.missing_attr is still a ResourceRef (not resolved), so ?? kicks in
        // Actually, resource refs remain as ResourceRef until resolution, so the left side IS a ResourceRef
        assert_eq!(
            cidr,
            Some(&Value::String("10.0.1.0/24".to_string())),
            "?? should return default when left is an unresolved ResourceRef"
        );
    }

    #[test]
    fn exports_cross_file_binding_detection() {
        // Simulate cross-file: exports.crn parsed WITHOUT the let binding
        let exports_input = r#"
exports {
  vpc_id = vpc.vpc_id
}
"#;
        let exports_parsed = parse(exports_input, &ProviderContext::default()).unwrap();
        eprintln!("export_params: {:?}", exports_parsed.export_params);
        assert_eq!(exports_parsed.export_params.len(), 1);
        // Check if the value is a ResourceRef
        let value = exports_parsed.export_params[0].value.as_ref().unwrap();
        eprintln!("value: {:?}", value);
        let is_ref = matches!(value, Value::ResourceRef { .. });
        eprintln!("is_ref: {}", is_ref);

        // Now simulate merged ParsedFile with binding from main.crn
        let main_input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = '10.0.0.0/16'
}
"#;
        let main_parsed = parse(main_input, &ProviderContext::default()).unwrap();

        // Merge like config_loader does
        let mut merged = main_parsed;
        merged.export_params.extend(exports_parsed.export_params);

        let unused = crate::validation::check_unused_bindings(&merged);
        assert!(
            unused.is_empty(),
            "vpc should not be unused when referenced from exports in a separate file, got: {:?}",
            unused
        );
    }

    #[test]
    fn coalesce_operator_returns_left_when_resolved() {
        let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.ec2.Vpc {
  cidr_block = '10.1.0.0/16' ?? '10.0.0.0/16'
}
"#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let cidr = parsed.resources[0].get_attr("cidr_block");
        assert_eq!(
            cidr,
            Some(&Value::String("10.1.0.0/16".to_string())),
            "?? should return left when it's resolved"
        );
    }

    #[test]
    fn upstream_state_refs_emit_no_parser_warnings() {
        // Field validity against upstream `exports { }` is now checked
        // statically by the `upstream_exports` module. The parser itself
        // stays silent about upstream_state references — the old "validate
        // does not inspect" soft warning is gone.
        let input = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }
            let network = upstream_state {
                source = "../network"
            }

            for name, _ in orgs.accounts {
                awscc.ec2.Vpc {
                    name = name
                    cidr_block = '10.0.0.0/16'
                }
            }

            awscc.ec2.SecurityGroup {
                group_description = "Web SG"
                vpc_id = network.vpc_id
            }
        "#;

        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let upstream_warnings: Vec<&ParseWarning> = parsed
            .warnings
            .iter()
            .filter(|w| w.message.contains("upstream_state"))
            .collect();
        assert!(
            upstream_warnings.is_empty(),
            "parser should emit no upstream_state warnings, got: {:?}",
            upstream_warnings
        );
        assert!(
            parsed
                .warnings
                .iter()
                .all(|w| !w.message.contains("known after apply")),
            "deferred for-iterable must no longer emit 'known after apply', got: {:?}",
            parsed.warnings
        );
    }

    #[test]
    fn expand_deferred_for_with_remote_bindings() {
        // Parse a for-expression that references an upstream_state list.
        // Initially deferred (no remote values available at parse time).
        // Then expand with remote_bindings and verify concrete resources are created.
        let input = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }

            for account_id in orgs.accounts {
                awscc.sso.Assignment {
                    instance_arn = 'arn:aws:sso:::instance/ssoins-12345'
                    target_id = account_id
                    target_type = 'AWS_ACCOUNT'
                }
            }
        "#;

        let mut parsed = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(parsed.deferred_for_expressions.len(), 1);
        assert_eq!(parsed.resources.len(), 0, "no resources before expansion"); // allow: direct — fixture test inspection

        // Simulate loading upstream_state with actual values
        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::List(vec![
                Value::String("111111111111".to_string()),
                Value::String("222222222222".to_string()),
            ]),
        );
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        // Expand deferred for-expressions
        parsed.expand_deferred_for_expressions(&remote_bindings);

        // Deferred should be resolved
        assert_eq!(
            parsed.deferred_for_expressions.len(),
            0,
            "deferred should be empty after expansion"
        );
        // Warning should be removed
        assert!(
            parsed.warnings.is_empty(),
            "warning should be removed after expansion, got: {:?}",
            parsed.warnings
        );
        // Two concrete resources should be generated
        assert_eq!(
            parsed.resources.len(), // allow: direct — fixture test inspection
            2,
            "should have 2 expanded resources"
        );

        // Verify the expanded resources have substituted values
        let r0 = &parsed.resources[0];
        assert_eq!(r0.id.resource_type, "sso.Assignment");
        let target_id_0 = r0.get_attr("target_id");
        assert_eq!(
            target_id_0,
            Some(&Value::String("111111111111".to_string())),
            "target_id should be substituted with actual account ID"
        );

        let r1 = &parsed.resources[1];
        let target_id_1 = r1.get_attr("target_id");
        assert_eq!(
            target_id_1,
            Some(&Value::String("222222222222".to_string())),
        );
    }

    #[test]
    fn expand_deferred_for_no_remote_data_stays_deferred() {
        let input = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }

            for account_id in orgs.accounts {
                awscc.sso.Assignment {
                    target_id = account_id
                }
            }
        "#;

        let mut parsed = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(parsed.deferred_for_expressions.len(), 1);

        // Empty remote_bindings — upstream hasn't been applied yet
        let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        parsed.expand_deferred_for_expressions(&remote_bindings);

        // Should remain deferred
        assert_eq!(
            parsed.deferred_for_expressions.len(),
            1,
            "should stay deferred when remote data not available"
        );
        assert_eq!(parsed.resources.len(), 0); // allow: direct — fixture test inspection
    }

    #[test]
    fn expand_deferred_for_map_binding_substitutes_key_and_value() {
        // Map binding `for k, v in orgs.accounts` should expand each entry with
        // both the key and value variables available.
        let input = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }

            for name, account_id in orgs.accounts {
                awscc.sso.Assignment {
                    target_id = account_id
                    target_name = name
                }
            }
        "#;

        let mut parsed = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(parsed.deferred_for_expressions.len(), 1);

        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut accounts: IndexMap<String, Value> = IndexMap::new();
        accounts.insert(
            "prod".to_string(),
            Value::String("111111111111".to_string()),
        );
        accounts.insert("dev".to_string(), Value::String("222222222222".to_string()));
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert("accounts".to_string(), Value::Map(accounts));
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        parsed.expand_deferred_for_expressions(&remote_bindings);

        assert_eq!(parsed.deferred_for_expressions.len(), 0);
        assert_eq!(parsed.resources.len(), 2); // allow: direct — fixture test inspection

        // Verify both key and value are substituted.
        let mut by_name: HashMap<String, &Resource> = HashMap::new();
        for r in &parsed.resources {
            if let Some(Value::String(s)) = r.get_attr("target_name") {
                by_name.insert(s.clone(), r);
            }
        }
        let prod = by_name.get("prod").expect("prod entry");
        assert_eq!(
            prod.get_attr("target_id"),
            Some(&Value::String("111111111111".to_string()))
        );
        let dev = by_name.get("dev").expect("dev entry");
        assert_eq!(
            dev.get_attr("target_id"),
            Some(&Value::String("222222222222".to_string()))
        );
    }

    #[test]
    fn expand_deferred_for_indexed_binding_substitutes_index_and_value() {
        // Indexed binding `for (i, x) in list` must substitute BOTH the index
        // and value variables. Prior to the fix both vars shared the same
        // placeholder, causing the index to receive the item value.
        let input = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }

            for (i, account_id) in orgs.accounts {
                awscc.sso.Assignment {
                    target_id = account_id
                    position = i
                }
            }
        "#;

        let mut parsed = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(parsed.deferred_for_expressions.len(), 1);

        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::List(vec![
                Value::String("111111111111".to_string()),
                Value::String("222222222222".to_string()),
            ]),
        );
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        parsed.expand_deferred_for_expressions(&remote_bindings);

        assert_eq!(parsed.resources.len(), 2); // allow: direct — fixture test inspection
        assert_eq!(
            parsed.resources[0].get_attr("target_id"),
            Some(&Value::String("111111111111".to_string()))
        );
        assert_eq!(
            parsed.resources[0].get_attr("position"),
            Some(&Value::Int(0)),
            "index should be 0, not the item value"
        );
        assert_eq!(
            parsed.resources[1].get_attr("target_id"),
            Some(&Value::String("222222222222".to_string()))
        );
        assert_eq!(
            parsed.resources[1].get_attr("position"),
            Some(&Value::Int(1))
        );
    }

    #[test]
    fn expand_deferred_for_substitutes_placeholder_inside_interpolation() {
        // The loop var may appear inside a string interpolation like "acct-${id}".
        // Placeholder substitution must recurse into Value::Interpolation parts,
        // otherwise the rendered resource ships the raw placeholder string.
        let input = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }

            for account_id in orgs.accounts {
                awscc.sso.Assignment {
                    target_id = account_id
                    label = "acct-${account_id}"
                }
            }
        "#;

        let mut parsed = parse(input, &ProviderContext::default()).unwrap();
        assert_eq!(parsed.deferred_for_expressions.len(), 1);

        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::List(vec![Value::String("111111111111".to_string())]),
        );
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        parsed.expand_deferred_for_expressions(&remote_bindings);
        assert_eq!(parsed.resources.len(), 1); // allow: direct — fixture test inspection

        // label must have the placeholder substituted in the interpolation.
        let label = parsed.resources[0].get_attr("label");
        let rendered = match label {
            Some(Value::Interpolation(parts)) => {
                let mut s = String::new();
                for p in parts {
                    match p {
                        crate::resource::InterpolationPart::Literal(lit) => s.push_str(lit),
                        crate::resource::InterpolationPart::Expr(Value::String(v)) => s.push_str(v),
                        _ => s.push_str("<expr>"),
                    }
                }
                s
            }
            Some(Value::String(s)) => s.clone(),
            other => panic!("unexpected label shape: {:?}", other),
        };
        assert!(
            rendered.contains("111111111111"),
            "interpolation should contain substituted account id, got: {}",
            rendered
        );
        assert!(
            !rendered.contains(DEFERRED_UPSTREAM_PLACEHOLDER),
            "placeholder must not leak into rendered label, got: {}",
            rendered
        );
    }

    #[test]
    fn expand_deferred_for_simple_binding_with_map_iterable_warns() {
        // Simple binding but upstream resolves to a map — mismatch should warn
        // and leave deferred.
        let input = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }

            for account_id in orgs.accounts {
                awscc.sso.Assignment {
                    target_id = account_id
                }
            }
        "#;

        let mut parsed = parse(input, &ProviderContext::default()).unwrap();

        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut accounts: IndexMap<String, Value> = IndexMap::new();
        accounts.insert(
            "prod".to_string(),
            Value::String("111111111111".to_string()),
        );
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert("accounts".to_string(), Value::Map(accounts));
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        parsed.expand_deferred_for_expressions(&remote_bindings);

        assert_eq!(
            parsed.resources.len(), // allow: direct — fixture test inspection
            0,
            "simple binding with map iterable should not silently expand"
        );
        assert_eq!(parsed.deferred_for_expressions.len(), 1);
        assert!(
            parsed
                .warnings
                .iter()
                .any(|w| w.message.contains("expected list")),
            "should warn about list vs map shape mismatch, got: {:?}",
            parsed.warnings
        );
    }

    #[test]
    fn expand_deferred_for_map_binding_with_list_iterable_warns() {
        // Map binding but upstream resolves to a list — mismatch should produce
        // a warning and leave the for-expression deferred (do not silently expand).
        let input = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }

            for name, account_id in orgs.accounts {
                awscc.sso.Assignment {
                    target_id = account_id
                }
            }
        "#;

        let mut parsed = parse(input, &ProviderContext::default()).unwrap();

        let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let mut orgs_attrs = HashMap::new();
        orgs_attrs.insert(
            "accounts".to_string(),
            Value::List(vec![
                Value::String("111111111111".to_string()),
                Value::String("222222222222".to_string()),
            ]),
        );
        remote_bindings.insert("orgs".to_string(), orgs_attrs);

        parsed.expand_deferred_for_expressions(&remote_bindings);

        // Mismatch: should NOT expand silently with numeric indices
        assert_eq!(
            parsed.resources.len(), // allow: direct — fixture test inspection
            0,
            "map binding with list iterable should not silently expand"
        );
        assert_eq!(
            parsed.deferred_for_expressions.len(),
            1,
            "should remain deferred on shape mismatch"
        );
        assert!(
            parsed
                .warnings
                .iter()
                .any(|w| w.message.contains("expected map") || w.message.contains("shape")),
            "should warn about shape mismatch, got: {:?}",
            parsed.warnings
        );
        // The parse-time "not yet available" warning should be replaced by the
        // more specific shape-mismatch warning (not kept alongside).
        assert!(
            !parsed
                .warnings
                .iter()
                .any(|w| w.message.contains("not yet available")
                    || w.message.contains("validate does not inspect")),
            "parse-time warning should be replaced, got: {:?}",
            parsed.warnings
        );
    }

    #[test]
    fn parses_upstream_state_expr_with_source() {
        let input = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).expect("parse should succeed");
        assert_eq!(parsed.upstream_states.len(), 1);
        let us = &parsed.upstream_states[0];
        assert_eq!(us.binding, "orgs");
        assert_eq!(us.source, std::path::PathBuf::from("../organizations"));
    }

    #[test]
    fn old_top_level_upstream_state_syntax_is_rejected() {
        // The pre-#1926 form `upstream_state "name" { ... }` was a top-level
        // statement; with the let-binding form it should no longer parse.
        let input = r#"
            upstream_state "orgs" {
                source = "../organizations"
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_err(),
            "old top-level upstream_state syntax must be rejected, got: {:?}",
            result.ok().map(|p| p.upstream_states)
        );
    }

    #[test]
    fn remote_state_keyword_is_no_longer_recognized() {
        let input = r#"
            let orgs = remote_state { path = "./foo.json" }
        "#;
        let err = parse(input, &ProviderContext::default())
            .expect_err("remote_state must be a parse error now");
        let msg = err.to_string();
        assert!(
            msg.contains("remote_state") && msg.contains("upstream_state"),
            "error should guide users to upstream_state, got: {msg}",
        );
    }

    #[test]
    fn upstream_state_missing_source_is_error() {
        let input = r#"let orgs = upstream_state { }"#;
        let err = parse(input, &ProviderContext::default())
            .expect_err("missing source must be a parse error");
        let msg = err.to_string();
        assert!(
            msg.contains("upstream_state") && msg.contains("source") && msg.contains("orgs"),
            "error should mention upstream_state, binding, and source: {msg}",
        );
    }

    #[test]
    fn upstream_state_source_must_be_string() {
        let input = r#"let orgs = upstream_state { source = 42 }"#;
        let err = parse(input, &ProviderContext::default())
            .expect_err("non-string source must be a parse error");
        let msg = err.to_string();
        assert!(
            msg.contains("source") && msg.contains("orgs"),
            "error should mention source and binding: {msg}",
        );
    }

    #[test]
    fn upstream_state_unknown_attribute_is_error() {
        let input = r#"
            let orgs = upstream_state {
                source = "../foo"
                backend = "s3"
            }
        "#;
        let err = parse(input, &ProviderContext::default())
            .expect_err("unknown attribute must be a parse error");
        let msg = err.to_string();
        assert!(
            msg.contains("backend") && msg.contains("orgs"),
            "error should mention the unknown attribute and binding: {msg}",
        );
    }

    #[test]
    fn upstream_state_duplicate_binding_is_error() {
        let input = r#"
            let orgs = upstream_state { source = "../a" }
            let orgs = upstream_state { source = "../b" }
        "#;
        let err = parse(input, &ProviderContext::default())
            .expect_err("duplicate upstream_state binding must be a parse error");
        match &err {
            ParseError::DuplicateBinding { name, .. } => {
                assert_eq!(name, "orgs");
            }
            other => panic!("Expected DuplicateBinding error, got: {other}"),
        }
    }

    // A dotted reference `orgs.accounts` is only valid when `orgs` is declared
    // somewhere in scope (`let`, `upstream_state`, `read`, module import,
    // function, or for/if structural binding). Referring to a name that isn't
    // bound anywhere must be a hard error, not a deferred warning.

    #[test]
    fn undefined_identifier_in_for_iterable_is_error() {
        let input = r#"
            for name, account_id in orgs.accounts {
                aws.s3_bucket {
                    name = name
                }
            }
        "#;
        // Iterable-binding validation runs in `check_identifier_scope`
        // on the merged directory-level `ParsedFile`, so that cross-file
        // `upstream_state` bindings in sibling files aren't rejected during
        // per-file parsing.
        let parsed = parse(input, &ProviderContext::default())
            .expect("single-file parse must not reject cross-file iterables");
        let errs = check_identifier_scope(&parsed);
        assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
        match &errs[0] {
            ParseError::UndefinedIdentifier { name, .. } => {
                assert_eq!(name, "orgs");
            }
            other => panic!("Expected UndefinedIdentifier, got: {other}"),
        }
    }

    #[test]
    fn undefined_identifier_error_suggests_close_match() {
        // Regression for #2038. When a typo has a close edit-distance match
        // among the in-scope bindings, the error should name it so the user
        // doesn't have to guess which binding they meant.
        let input = r#"
            let orgs = upstream_state { source = "../a" }
            for _, id in org.accounts {
                aws.s3_bucket {
                    name = id
                }
            }
        "#;
        let parsed = parse(input, &ProviderContext::default())
            .expect("single-file parse must not reject cross-file iterables");
        let errs = check_identifier_scope(&parsed);
        assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
        let msg = errs[0].to_string();
        assert!(
            msg.contains("`org`"),
            "error should quote the unknown name, got: {msg}"
        );
        assert!(
            msg.contains("Did you mean `orgs`") || msg.contains("Did you mean 'orgs'"),
            "error should suggest the close match 'orgs', got: {msg}"
        );
    }

    #[test]
    fn undefined_identifier_error_lists_in_scope_names_without_close_match() {
        // When nothing is close, fall back to listing the concrete in-scope
        // names so the reader learns what _is_ available. The abstract
        // "no let/upstream_state/..." kind enumeration alone is noise.
        let input = r#"
            let orgs = upstream_state { source = "../a" }
            let admins = upstream_state { source = "../b" }
            for _, id in xyzzy.accounts {
                aws.s3_bucket {
                    name = id
                }
            }
        "#;
        let parsed = parse(input, &ProviderContext::default())
            .expect("single-file parse must not reject cross-file iterables");
        let errs = check_identifier_scope(&parsed);
        assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
        let msg = errs[0].to_string();
        assert!(
            msg.contains("`xyzzy`"),
            "error should quote the unknown name, got: {msg}"
        );
        assert!(
            msg.contains("orgs") && msg.contains("admins"),
            "error should list in-scope names (orgs, admins), got: {msg}"
        );
        assert!(
            !msg.contains("Did you mean"),
            "no close match exists; there should be no 'Did you mean' line, got: {msg}"
        );
    }

    #[test]
    fn bare_identifier_iterable_is_reported_as_undefined_not_string() {
        // Regression for #2101. When the iterable is a bare undeclared
        // identifier — `for ... in org { ... }` rather than the dotted
        // `org.accounts` — the parser previously reported
        // `iterable is string "org" (expected map)`, calling the identifier
        // a string and leaving the user with no did-you-mean.
        //
        // The fix records these as `DeferredForExpression` so
        // `check_identifier_scope` validates them against the merged
        // directory-wide binding set (mirrors the dotted-form path). That
        // gives us cross-file visibility for the did-you-mean candidates.
        let input = r#"
            let orgs = upstream_state { source = "../a" }
            for _, id in org {
                aws.s3_bucket {
                    name = id
                }
            }
        "#;
        let parsed = parse(input, &ProviderContext::default())
            .expect("single-file parse must not reject bare-iterable identifiers; the cross-file check runs later");
        let errs = check_identifier_scope(&parsed);
        assert_eq!(errs.len(), 1, "expected one error, got {errs:?}");
        let err = &errs[0];
        let msg = err.to_string();
        assert!(
            matches!(err, ParseError::UndefinedIdentifier { .. }),
            "expected UndefinedIdentifier, got: {err:?}"
        );
        assert!(
            msg.contains("`org`"),
            "error should quote the identifier, got: {msg}"
        );
        assert!(
            !msg.contains("\"org\""),
            "error must not render the identifier as a quoted string literal, got: {msg}"
        );
        assert!(
            msg.contains("Did you mean `orgs`") || msg.contains("Did you mean 'orgs'"),
            "error should suggest the close match 'orgs' via #2038 plumbing, got: {msg}"
        );
    }

    #[test]
    fn forward_reference_to_later_let_is_allowed() {
        // `foo.id` refers to `let foo = ...` declared after the first resource.
        // This is a legitimate forward reference that the second-pass resolver
        // handles.
        let input = r#"
            let bucket = aws.s3_bucket {
                name = foo.id
            }
            let foo = aws.s3_bucket {
                name = "foo-bucket"
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_ok(),
            "Forward reference to later `let` must still parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn backward_reference_to_resource_attr_is_allowed() {
        // `bucket.id` — `bucket` is defined; `id` is populated after apply.
        // This is the legitimate "known after apply" case.
        let input = r#"
            let bucket = aws.s3_bucket {
                name = "my-bucket"
            }
            aws.s3_bucket_policy {
                name = "policy"
                bucket_name = bucket.id
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_ok(),
            "Reference to declared binding's attribute must parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn for_discard_pattern_simple_parses() {
        // `for _ in xs` should parse — the loop variable is intentionally unused.
        let input = r#"
            for _ in [1, 2, 3] {
                awscc.ec2.Vpc {
                    cidr_block = '10.0.0.0/16'
                }
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_ok(),
            "discard in simple for-binding must parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn for_discard_pattern_map_key_parses() {
        // `for _, v in m` — discard the map key, use only the value.
        let input = r#"
            let things = { a = 1, b = 2 }
            for _, value in things {
                awscc.ec2.Vpc {
                    cidr_block = '10.0.0.0/16'
                }
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_ok(),
            "discard in map-form key position must parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn for_discard_pattern_map_value_parses() {
        let input = r#"
            let things = { a = 1, b = 2 }
            for key, _ in things {
                awscc.ec2.Vpc {
                    cidr_block = '10.0.0.0/16'
                }
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_ok(),
            "discard in map-form value position must parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn for_discard_pattern_indexed_parses() {
        let input = r#"
            for (_, item) in [1, 2, 3] {
                awscc.ec2.Vpc {
                    cidr_block = '10.0.0.0/16'
                }
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_ok(),
            "discard in indexed-form must parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn for_discard_pattern_cannot_be_referenced() {
        // Using `_` on the RHS should error — it's not a binding, it's a
        // discard marker. This mirrors `let _ = expr`.
        let input = r#"
            for _, v in { a = 1 } {
                awscc.ec2.Vpc {
                    name = _
                    cidr_block = '10.0.0.0/16'
                }
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_err(),
            "referencing a discard binding should error, got: {:?}",
            result
        );
    }

    #[test]
    fn for_unused_binding_warns_simple() {
        // Simple-form loop variable never referenced inside the body — warn.
        let input = r#"
            for item in [1, 2, 3] {
                awscc.ec2.Vpc {
                    cidr_block = '10.0.0.0/16'
                }
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let unused: Vec<_> = parsed
            .warnings
            .iter()
            .filter(|w| w.message.contains("unused") && w.message.contains("item"))
            .collect();
        assert_eq!(
            unused.len(),
            1,
            "expected one unused-for-binding warning, got: {:?}",
            parsed.warnings
        );
    }

    #[test]
    fn for_used_binding_no_warning() {
        // Binding is referenced in body — no warning.
        let input = r#"
            for item in [1, 2, 3] {
                awscc.ec2.Vpc {
                    name = item
                    cidr_block = '10.0.0.0/16'
                }
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        assert!(
            !parsed
                .warnings
                .iter()
                .any(|w| w.message.contains("unused") && w.message.contains("item")),
            "expected no unused warning when binding is used, got: {:?}",
            parsed.warnings
        );
    }

    #[test]
    fn for_unused_map_key_warns_only_key() {
        // Only the map key is unused — warn for key, not value.
        let input = r#"
            let things = { a = 1, b = 2 }
            for name, account_id in things {
                awscc.ec2.Vpc {
                    cidr_block = account_id
                }
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let unused: Vec<_> = parsed
            .warnings
            .iter()
            .filter(|w| w.message.contains("unused"))
            .collect();
        assert_eq!(
            unused.len(),
            1,
            "expected one warning for unused key, got: {:?}",
            parsed.warnings
        );
        assert!(
            unused[0].message.contains("name"),
            "expected warning to mention 'name', got: {}",
            unused[0].message
        );
        assert!(
            !unused[0].message.contains("account_id"),
            "warning should not mention used binding, got: {}",
            unused[0].message
        );
    }

    #[test]
    fn for_discard_binding_no_unused_warning() {
        // `_` discard should suppress the unused-warning check.
        let input = r#"
            let things = { a = 1, b = 2 }
            for _, account_id in things {
                awscc.ec2.Vpc {
                    cidr_block = account_id
                }
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        assert!(
            !parsed.warnings.iter().any(|w| w.message.contains("unused")),
            "discard binding should suppress unused warning, got: {:?}",
            parsed.warnings
        );
    }

    #[test]
    fn reference_to_upstream_state_binding_is_allowed() {
        // `orgs` IS declared via upstream_state. The field (`accounts`) may
        // not yet be loaded — that stays as a deferred warning, not an error.
        let input = r#"
            let orgs = upstream_state {
                source = "../organizations"
            }
            for name, account_id in orgs.accounts {
                aws.s3_bucket {
                    name = name
                }
            }
        "#;
        let result = parse(input, &ProviderContext::default());
        assert!(
            result.is_ok(),
            "Reference to upstream_state binding must parse: {:?}",
            result.err()
        );
    }

    /// Issue #2094: distinguish quoted string literals from bare identifiers
    /// and namespaced identifiers at the parser level, so downstream enum
    /// diagnostics can report shape mismatches ("got a string literal") vs.
    /// variant mismatches ("invalid enum variant").
    #[test]
    fn string_literal_paths_distinguish_quoted_from_bare_and_namespaced() {
        let input = r#"
            let a = aws.sso_admin.principal_assignment {
                target_type = "aaa"
            }

            let b = aws.sso_admin.principal_assignment {
                target_type = AWS_ACCOUNT
            }

            let c = aws.sso_admin.principal_assignment {
                target_type = awscc.sso.Assignment.TargetType.AWS_ACCOUNT
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();

        let paths = &parsed.string_literal_paths;

        let quoted = StringLiteralPath {
            resource_id: ResourceId::with_provider("aws", "sso_admin.principal_assignment", "a"),
            attribute_chain: vec!["target_type".to_string()],
        };
        assert!(
            paths.contains(&quoted),
            "quoted literal `target_type = \"aaa\"` must be recorded; paths = {:?}",
            paths
        );

        let bare = StringLiteralPath {
            resource_id: ResourceId::with_provider("aws", "sso_admin.principal_assignment", "b"),
            attribute_chain: vec!["target_type".to_string()],
        };
        assert!(
            !paths.contains(&bare),
            "bare identifier `target_type = AWS_ACCOUNT` must NOT be recorded as a string literal; paths = {:?}",
            paths
        );

        let namespaced = StringLiteralPath {
            resource_id: ResourceId::with_provider("aws", "sso_admin.principal_assignment", "c"),
            attribute_chain: vec!["target_type".to_string()],
        };
        assert!(
            !paths.contains(&namespaced),
            "namespaced identifier must NOT be recorded as a string literal; paths = {:?}",
            paths
        );
    }

    #[test]
    fn string_literal_paths_record_nested_block_attributes() {
        // Nested block `rules { protocol = "tcp" }` should produce a path
        // with the block name and its per-occurrence index, matching how the
        // schema validator walks list-of-struct values.
        let input = r#"
            let sg = aws.ec2.SecurityGroup {
                name = "sg-1"
                rules {
                    protocol = "tcp"
                }
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();

        let expected = StringLiteralPath {
            resource_id: ResourceId::with_provider("aws", "ec2.SecurityGroup", "sg"),
            attribute_chain: vec!["rules".to_string(), "0".to_string(), "protocol".to_string()],
        };
        assert!(
            parsed.string_literal_paths.contains(&expected),
            "nested-block string literal must be recorded with index path; paths = {:?}",
            parsed.string_literal_paths
        );
    }

    #[test]
    fn string_literal_paths_skip_interpolated_strings() {
        // An interpolated string is not a "plain" literal — users who write
        // "${x}" are constructing a value, not typing an enum by mistake.
        let input = r#"
            let x = "env"
            let r = aws.s3_bucket {
                name = "bucket-${x}"
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();

        let interpolated = StringLiteralPath {
            resource_id: ResourceId::with_provider("aws", "s3_bucket", "r"),
            attribute_chain: vec!["name".to_string()],
        };
        assert!(
            !parsed.string_literal_paths.contains(&interpolated),
            "interpolated strings must not be tagged as plain literals; paths = {:?}",
            parsed.string_literal_paths
        );
    }

    /// The payload of `Value::Map` must preserve the source order of the
    /// keys the user wrote — top-level map literals included.
    #[test]
    fn value_map_preserves_insertion_order() {
        let input = r#"
            let m = {
                z_first = "1"
                a_second = "2"
                m_third = "3"
                b_fourth = "4"
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let Some(Value::Map(map)) = parsed.variables.get("m") else {
            panic!("expected variables['m'] to be a Value::Map");
        };
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["z_first", "a_second", "m_third", "b_fourth"],
            "Value::Map must preserve source key order; got {keys:?}"
        );
    }

    /// `ProviderConfig.default_tags` must preserve the source order in
    /// which the user wrote tag keys. The map is extracted from a
    /// `default_tags = { ... }` block, so the same `Value::Map`
    /// guarantee applies.
    #[test]
    fn provider_config_default_tags_preserve_insertion_order() {
        let input = r#"
            provider test {
                source = "x/y"
                version = "0.1"
                region = "ap-northeast-1"
                default_tags = {
                    z_team = "infra"
                    a_env = "prod"
                    m_owner = "ops"
                }
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let pc = parsed
            .providers
            .first()
            .expect("expected one provider config");
        let keys: Vec<&str> = pc.default_tags.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["z_team", "a_env", "m_owner"],
            "ProviderConfig.default_tags must preserve source key order; got {keys:?}"
        );
    }

    /// `ProviderConfig.attributes` must preserve source order so that
    /// anything re-rendering provider blocks (formatter, diagnostics)
    /// sees a deterministic order.
    #[test]
    fn provider_config_attributes_preserve_insertion_order() {
        let input = r#"
            provider test {
                source = "x/y"
                version = "0.1"
                z_extra = "1"
                a_extra = "2"
                m_extra = "3"
                region = "ap-northeast-1"
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let pc = parsed
            .providers
            .first()
            .expect("expected one provider config");
        let keys: Vec<&str> = pc.attributes.keys().map(String::as_str).collect();
        // `source` and `version` are stripped from `attributes` (extracted
        // separately into ProviderConfig fields), so the surviving keys
        // are the user-authored order minus those two.
        assert_eq!(
            keys,
            vec!["z_extra", "a_extra", "m_extra", "region"],
            "ProviderConfig.attributes must preserve source key order; got {keys:?}"
        );
    }

    /// `ParsedFile.variables` must preserve the order in which top-level
    /// `let` bindings were declared so that iteration matches source
    /// order. Later bindings can reference earlier ones.
    #[test]
    fn parsed_file_variables_preserve_insertion_order() {
        let input = r#"
            let z_first = "1"
            let a_second = "2"
            let m_third = "3"
            let b_fourth = "4"
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let keys: Vec<&str> = parsed.variables.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["z_first", "a_second", "m_third", "b_fourth"],
            "ParsedFile.variables must preserve source order; got {keys:?}"
        );
    }

    /// A nested block's attributes must surface in source order on the
    /// `Value::Map` payload, end-to-end through the parser.
    #[test]
    fn nested_block_value_map_preserves_insertion_order() {
        let input = r#"
            provider test {
                source = "x/y"
                version = "0.1"
                region = "ap-northeast-1"
            }
            let r = test.r.res {
                name = "x"
                nested {
                    z_first = "1"
                    a_second = "2"
                    m_third = "3"
                }
            }
        "#;
        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let resource = parsed
            .resources
            .first()
            .expect("expected one resource binding");
        let nested = resource
            .get_attr("nested")
            .expect("expected `nested` attribute");
        // Nested blocks are wrapped in a List<Map> by the parser.
        let Value::List(blocks) = nested else {
            panic!("expected nested blocks to be a List, got {nested:?}");
        };
        let block = blocks.first().expect("expected one nested block");
        let Value::Map(map) = block else {
            panic!("expected nested block to be a Value::Map, got {block:?}");
        };
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["z_first", "a_second", "m_third"],
            "nested block Value::Map must preserve source key order; got {keys:?}"
        );
    }
}
