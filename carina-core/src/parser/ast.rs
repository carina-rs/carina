//! Parser AST types and the public `ParsedFile` result.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::error::ParseWarning;
use super::expressions::for_expr::ForBinding;
use super::expressions::validate_expr::CompareOp;
use super::util::snake_to_pascal;
use crate::resource::{Resource, ResourceId, Value};
use crate::version_constraint::VersionConstraint;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

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
    /// Sentinel for inference failure: an unannotated export whose
    /// rhs could not be statically typed. Produced *only* by
    /// `apply_inference`, never by the parser. Type-comparison
    /// predicates reject `Unknown` against any concrete receiver, so
    /// the `inference_errors` channel surfaces the actionable
    /// "type annotation required" message instead of a cascade of
    /// "missing export" diagnostics. See #2360 stage 2.
    Unknown,
}

impl TypeExpr {
    /// Project away the [`TypeExpr::Unknown`] sentinel: returns
    /// `Some(self)` for any concrete type, `None` for `Unknown`. Used
    /// when a downstream consumer has no use for sentinel-bearing
    /// entries (plan display, upstream-export forwarding) and prefers
    /// the legacy "no static type" `None` shape.
    pub fn into_known(self) -> Option<TypeExpr> {
        match self {
            TypeExpr::Unknown => None,
            other => Some(other),
        }
    }
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
            TypeExpr::Unknown => write!(f, "<unknown>"),
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

// `CompareOp` lives in `expressions::validate_expr` (re-exported above).

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

/// An export parameter in an `exports { }` block, as produced by the
/// parser before any inference runs.
///
/// `type_expr` is `Option<TypeExpr>` here because the user may have
/// omitted the annotation. The loader then runs `apply_inference`
/// (#2360 stage 2) to resolve the effective type and emits
/// [`InferredExportParam`] downstream.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedExportParam {
    pub name: String,
    pub type_expr: Option<TypeExpr>,
    pub value: Option<Value>,
}

/// Alias kept so the parser's own construct sites (which always
/// produce the parser-phase shape) read naturally as
/// `ExportParameter`; downstream consumers should use the explicit
/// [`ParsedExportParam`] / [`InferredExportParam`] names.
pub type ExportParameter = ParsedExportParam;

/// Phase-agnostic accessor over an export parameter. Implemented by both
/// [`ParsedExportParam`] (parser phase) and [`InferredExportParam`]
/// (post-loader phase) so helpers like `check_unused_bindings` work
/// uniformly across both shapes.
pub trait ExportParamLike {
    fn name(&self) -> &str;
    fn value(&self) -> Option<&Value>;
}

impl ExportParamLike for ParsedExportParam {
    fn name(&self) -> &str {
        &self.name
    }
    fn value(&self) -> Option<&Value> {
        self.value.as_ref()
    }
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

/// An export parameter as seen post-inference (#2360 stage 2).
///
/// `type_expr` is bare `TypeExpr` because every export carries a
/// definitive type by construction: the loader runs `apply_inference`
/// after parse + resolve, which fills in either the user's annotation,
/// the rhs-inferred type, or the [`TypeExpr::Unknown`] sentinel for
/// failed inference (paired with an entry in `LoadedConfig.inference_errors`).
#[derive(Debug, Clone, PartialEq)]
pub struct InferredExportParam {
    pub name: String,
    pub type_expr: TypeExpr,
    pub value: Option<Value>,
}

impl ExportParamLike for InferredExportParam {
    fn name(&self) -> &str {
        &self.name
    }
    fn value(&self) -> Option<&Value> {
        self.value.as_ref()
    }
}

/// Parse result, generic over the export-parameter shape.
///
/// Two phases share this struct via aliases (see [`ParsedFile`] and
/// [`InferredFile`]): the parser produces `File<ParsedExportParam>`
/// where `type_expr` is `Option<TypeExpr>`; the loader runs
/// `apply_inference` and yields `File<InferredExportParam>` where every
/// export's `type_expr` is bare. Every other field is identical between
/// phases. See `docs/specs/2026-05-03-typeexpr-stage2-design.md`.
#[derive(Debug, Clone)]
pub struct File<E> {
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
    /// Top-level export parameters (published to upstream_state consumers).
    /// Element type varies by phase — see [`ParsedExportParam`] / [`InferredExportParam`].
    pub export_params: Vec<E>,
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
}

/// Parse-phase file: exports retain their `Option<TypeExpr>` shape.
pub type ParsedFile = File<ParsedExportParam>;

/// Post-inference file: every export carries a bare [`TypeExpr`]
/// (possibly [`TypeExpr::Unknown`] for inference failures).
pub type InferredFile = File<InferredExportParam>;

impl<E> Default for File<E> {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            resources: Vec::new(),
            variables: IndexMap::new(),
            uses: Vec::new(),
            module_calls: Vec::new(),
            arguments: Vec::new(),
            attribute_params: Vec::new(),
            export_params: Vec::new(),
            backend: None,
            state_blocks: Vec::new(),
            user_functions: HashMap::new(),
            upstream_states: Vec::new(),
            requires: Vec::new(),
            structural_bindings: HashSet::new(),
            warnings: Vec::new(),
            deferred_for_expressions: Vec::new(),
        }
    }
}

impl<E> File<E> {
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
                        substitute_attrs(&mut resource, None, None, item);
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
                        substitute_attrs(&mut resource, Some(i as i64), None, item);
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
                        let address = crate::utils::map_key_address(&deferred.binding_name, key);
                        let mut resource = deferred.template_resource.clone();
                        resource.id.set_name(address.clone());
                        resource.binding = Some(address);
                        substitute_attrs(&mut resource, None, Some(key), val);
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

/// Run `substitute_placeholder` over every attribute of `resource`, then
/// canonicalize each attribute in place. The canonicalize step lives here
/// (and not at parse time) because once placeholders have been replaced
/// with concrete scalars, surrounding `Interpolation` shapes can collapse
/// to a `String` (#2227).
fn substitute_attrs(
    resource: &mut crate::resource::Resource,
    index: Option<i64>,
    key: Option<&str>,
    value: &Value,
) {
    for (_, attr) in resource.attributes.iter_mut() {
        substitute_placeholder(attr, index, key, value);
        attr.canonicalize_in_place();
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
pub(super) fn substitute_placeholder(
    v: &mut Value,
    index: Option<i64>,
    key: Option<&str>,
    value: &Value,
) {
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
