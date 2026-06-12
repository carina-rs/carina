//! Parser AST types and the public `ParsedFile` result.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::error::ParseWarning;
use super::expressions::for_expr::ForBinding;
use super::expressions::validate_expr::CompareOp;
use super::util::snake_to_pascal;
use crate::binding_index::IterableBindings;
use crate::resource::{
    Composition, ConcreteValue, DataSource, DeferredValue, Directives, GraphNode, LeafNode,
    Resource, ResourceId, UnknownReason, Value,
};
use crate::version_constraint::VersionConstraint;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

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

/// A resource yielded by [`File::iter_all_resources`], discriminated by
/// its typestate arm.
///
/// Part of the carina#3181 resource typestate split. The three typed
/// arms borrow from `File`'s typed slices ([`File::resources`] managed
/// rows, [`File::compositions`], [`File::data_sources`]); the
/// `Deferred` arm is a `for`-expression template body that still lives
/// as a legacy [`Resource`] in [`File::deferred_for_expressions`].
///
/// Read-only consumers reach the shared accessors via
/// [`ResourceRef::as_resource_like`] (or the `id` / `attributes` /
/// `binding` passthroughs); callers that branch on the kind match the
/// enum arm directly.
#[derive(Debug, Clone, Copy)]
pub enum ResourceRef<'a> {
    /// A top-level resource.
    Resource(&'a Resource),
    /// A top-level composition (module-expansion synthetic node).
    Composition(&'a Composition),
    /// A top-level data source (`read`-keyword resource).
    DataSource(&'a DataSource),
    /// The template body of a deferred `for` expression — always a
    /// resource (`for` bodies never carry `read` / composition).
    Deferred {
        resource: &'a Resource,
        deferred: &'a DeferredForExpression,
    },
}

impl<'a> ResourceRef<'a> {
    /// Stable identifier of this resource.
    pub fn id(&self) -> &'a ResourceId {
        match self {
            ResourceRef::Resource(r) => &r.id,
            ResourceRef::Composition(v) => &v.id,
            ResourceRef::DataSource(d) => &d.id,
            ResourceRef::Deferred { resource, .. } => &resource.id,
        }
    }

    /// Source-order preserving attribute map as `Value`s.
    ///
    /// Returned as a [`Cow`](std::borrow::Cow) because
    /// [`Composition`] stores its attributes typed as
    /// [`CompositionAttribute`](crate::resource::CompositionAttribute)
    /// since #3294, and must materialize a `Value`-typed view on
    /// demand. The other three variants ([`Resource`],
    /// [`DataSource`], [`Deferred`](Self::Deferred)) return a borrowed
    /// reference. Callers `.iter()` / `.contains_key()` / `.get()`
    /// through the `Cow` directly via `Deref`.
    pub fn attributes(&self) -> std::borrow::Cow<'a, IndexMap<String, Value>> {
        match self {
            ResourceRef::Resource(r) => std::borrow::Cow::Borrowed(&r.attributes),
            ResourceRef::Composition(v) => std::borrow::Cow::Owned(
                v.signature
                    .attributes
                    .iter()
                    .map(|(k, attr)| (k.clone(), attr.to_value()))
                    .collect(),
            ),
            ResourceRef::DataSource(d) => std::borrow::Cow::Borrowed(&d.attributes),
            ResourceRef::Deferred { resource, .. } => {
                std::borrow::Cow::Borrowed(&resource.attributes)
            }
        }
    }

    /// `let` binding name if any.
    pub fn binding(&self) -> Option<&'a str> {
        match self {
            ResourceRef::Resource(r) => r.binding.as_deref(),
            ResourceRef::Composition(v) => v.binding.as_deref(),
            ResourceRef::DataSource(d) => d.binding.as_deref(),
            ResourceRef::Deferred { resource, .. } => resource.binding.as_deref(),
        }
    }

    /// Binding names this resource depends on (via `ResourceRef` /
    /// `BindingRef` values in its attribute tree).
    ///
    /// Mirrors the field formerly exposed by
    /// `ResourceLike::dependency_bindings`; #3308 lifts this accessor
    /// onto `ResourceRef` so the trait can retire.
    pub fn dependency_bindings(&self) -> &'a std::collections::BTreeSet<String> {
        match self {
            ResourceRef::Resource(r) => &r.dependency_bindings,
            ResourceRef::Composition(v) => &v.dependency_bindings,
            ResourceRef::DataSource(d) => &d.dependency_bindings,
            ResourceRef::Deferred { resource, .. } => &resource.dependency_bindings,
        }
    }

    /// `directives` meta-argument block. [`Composition`] drops the
    /// field (no `prevent_destroy` on a synthetic node) — returns `None`
    /// for the `Virtual` arm.
    pub fn directives(&self) -> Option<&'a Directives> {
        match self {
            ResourceRef::Resource(r) => Some(&r.directives),
            ResourceRef::Composition(_) => None,
            ResourceRef::DataSource(d) => Some(&d.directives),
            ResourceRef::Deferred { resource, .. } => Some(&resource.directives),
        }
    }

    /// Parser-level set of attributes written as quoted string literals.
    pub fn quoted_string_attrs(&self) -> &'a HashSet<String> {
        match self {
            ResourceRef::Resource(r) => &r.quoted_string_attrs,
            ResourceRef::Composition(v) => &v.quoted_string_attrs,
            ResourceRef::DataSource(d) => &d.quoted_string_attrs,
            ResourceRef::Deferred { resource, .. } => &resource.quoted_string_attrs,
        }
    }

    /// The [`ResourceContext`] an old `iter_all_resources` caller would
    /// have seen — `Deferred` for a for-expression template, `Direct`
    /// otherwise.
    pub fn context(&self) -> ResourceContext<'a> {
        match self {
            ResourceRef::Deferred { deferred, .. } => ResourceContext::Deferred(deferred),
            _ => ResourceContext::Direct,
        }
    }

    /// Attributes projected to a `HashMap<String, Value>` — the
    /// lookup-shaped view consumed by schema validation.
    pub fn resolved_attributes(&self) -> HashMap<String, Value> {
        crate::resource::attrs_to_hashmap(&self.attributes())
    }
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
    /// Time duration. Surface form: `<integer><unit>` literal (`75min`,
    /// `1h`, `30s`); internal form: `Value::Concrete(ConcreteValue::Duration(std::time::Duration))`.
    Duration,
    /// Schema type identified by name (e.g., "ipv4_cidr", "ipv4_address", "arn")
    Simple(std::string::String),
    List(Box<TypeExpr>),
    Map(Box<TypeExpr>),
    /// Reference to a resource type (e.g., aws.vpc)
    Ref(ResourceTypePath),
    /// Dotted type path produced by the parser before provider schemas
    /// have been loaded enough to classify it as a resource ref or
    /// provider custom type.
    DottedUnresolved(ResourceTypePath),
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
    /// equal. A `Value::Concrete(ConcreteValue::Map)` satisfies a struct type when every field
    /// name appears as a key with a value that matches the field's type,
    /// with no extra keys.
    Struct {
        fields: Vec<(String, TypeExpr)>,
    },
    /// Singleton string literal type: `'dev'` accepts only the value
    /// `Value::Concrete(ConcreteValue::String("dev"))` (carina-rs/carina#2611). Composes with
    /// [`TypeExpr::Union`] to produce closed-set string types like
    /// `'dev' | 'prod'`, and with [`TypeExpr::List`] / `Map` to nest
    /// (`list('dev' | 'prod')`).
    StringLiteral(String),
    /// Union of two or more types: `T1 | T2 | ...`. A value matches
    /// the union if it matches at least one member type. Today the
    /// only grammar-reachable shape is unions of [`TypeExpr::StringLiteral`]
    /// — `'dev' | 'prod'` — but the AST shape stays general so future
    /// additions (`String | Int`, nullable types via `T | none`) drop
    /// in without another structural change. See carina-rs/carina#2611.
    Union(Vec<TypeExpr>),
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
            TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::List(_)
            | TypeExpr::Map(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::Struct { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Union(_) => Some(self),
        }
    }

    /// True when this `TypeExpr` represents a string-shaped value at
    /// runtime: bare `String`, a `Simple` named identity (typically a
    /// string-base custom like `AwsAccountId`), or a `SchemaType`
    /// (provider-defined string-typed identifier like
    /// `awscc.ec2.VpcId`). Callers use this to accept these in any
    /// string-compatible receiver position; the symmetric strictness
    /// in the opposite direction (`String → Custom{Specific}`) is
    /// enforced by `attr_type_demands_specific_custom` in the
    /// validation crate.
    pub fn is_string_shaped(&self) -> bool {
        matches!(
            self,
            TypeExpr::String
                | TypeExpr::Simple(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { .. }
        )
    }
}

impl std::fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeExpr::String => write!(f, "String"),
            TypeExpr::Bool => write!(f, "Bool"),
            TypeExpr::Int => write!(f, "Int"),
            TypeExpr::Float => write!(f, "Float"),
            TypeExpr::Duration => write!(f, "Duration"),
            TypeExpr::Simple(name) => write!(f, "{}", snake_to_pascal(name)),
            TypeExpr::List(inner) => write!(f, "list({})", inner),
            TypeExpr::Map(inner) => write!(f, "map({})", inner),
            TypeExpr::Ref(path) => write!(f, "{}", path),
            TypeExpr::DottedUnresolved(path) => write!(f, "{}", path),
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
            TypeExpr::StringLiteral(s) => write!(f, "'{}'", s),
            TypeExpr::Union(members) => {
                for (i, m) in members.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    write!(f, "{}", m)?;
                }
                Ok(())
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
    /// Duration literal (`75min`, `1h`, `30s`).
    Duration(std::time::Duration),
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
    /// The export's declared type, when one is available.
    ///
    /// The parser phase carries `Option<TypeExpr>` (the user may have
    /// omitted the annotation, inference fills it later); the post-
    /// inference phase carries a bare `TypeExpr` and the impl returns
    /// `Some(&type_expr)` unconditionally. Carina#3239's argument-side
    /// custom-type walk uses this so it can validate both phases through
    /// the same trait.
    fn type_expr_opt(&self) -> Option<&TypeExpr>;
    fn type_expr_opt_mut(&mut self) -> Option<&mut TypeExpr>;
}

impl ExportParamLike for ParsedExportParam {
    fn name(&self) -> &str {
        &self.name
    }
    fn value(&self) -> Option<&Value> {
        self.value.as_ref()
    }
    fn type_expr_opt(&self) -> Option<&TypeExpr> {
        self.type_expr.as_ref()
    }
    fn type_expr_opt_mut(&mut self) -> Option<&mut TypeExpr> {
        self.type_expr.as_mut()
    }
}

/// An address as written in a state block (`import { to = X 'addr' }`,
/// `removed { from = X 'addr' }`, `moved { from = X 'a', to = X 'b' }`).
///
/// The DSL surface form for these blocks has **no syntax for routing**
/// — there is no slot to specify which `provider_instance` the address
/// targets. The `provider_instance` of the matched resource (or state
/// row) is decided downstream by looking at the let-bound resource's
/// `directives { provider = ... }`.
///
/// This type is the **routing-agnostic** counterpart to
/// [`crate::resource::ResourceId`]:
///
/// - Its `Eq`/`Hash` includes only `(provider, resource_type, name)`.
///   Comparing two state-block addresses cannot silently mismatch on
///   a routing field that the DSL never specified.
/// - It does **not** implement `PartialEq<ResourceId>` and there is no
///   `From<&StateBlockAddress>` / `From<&ResourceId>` shortcut. The
///   only way to produce a `ResourceId` from a `StateBlockAddress` is
///   via a routing-lifting lookup against the plan / state (see
///   `resolve_import_target` and `find_desired_id` in
///   `carina-cli/src/wiring/mod.rs`), or — only when no match
///   exists — via the explicit
///   [`to_unrouted_resource_id`](Self::to_unrouted_resource_id)
///   escape hatch documented on the method itself.
///
/// This shape makes the carina#3324 bug class — comparing a
/// None-routed parsed address against a routed `ResourceId` and
/// silently missing — unrepresentable: the type signature forces every
/// consumer to perform the resolution step explicitly. A new consumer
/// added tomorrow cannot reach the buggy path; the compiler rejects
/// the comparison.
///
/// The cross-type equality guard is enforced by the type system:
///
/// ```compile_fail
/// use carina_core::parser::StateBlockAddress;
/// use carina_core::resource::ResourceId;
///
/// let addr = StateBlockAddress::new("aws", "route53.RecordSet", "r");
/// let routed = ResourceId::with_provider(
///     "aws",
///     "route53.RecordSet",
///     "r",
///     Some("management".to_string()),
/// );
///
/// // The compiler rejects this — `StateBlockAddress` does not
/// // implement `PartialEq<ResourceId>`. A new consumer cannot
/// // silently miss a routed-vs-unrouted match the way carina#3324
/// // did before the newtype existed.
/// let _ = addr == routed;
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct StateBlockAddress {
    pub provider: String,
    pub resource_type: String,
    pub name: crate::resource::ResourceName,
}

impl StateBlockAddress {
    /// Construct a `StateBlockAddress`. The `name` is canonicalized
    /// through [`crate::utils::canonicalize_map_key_address`] so all
    /// three DSL surface forms (`binding.key`, `binding['key']`,
    /// `binding["key"]`) collapse to the same value — the parser's
    /// `parse_state_block_address` does the same, and the type's
    /// `Eq`/`Hash` only match when the canonical form does. Without
    /// canonicalization at the constructor, a programmatic caller
    /// could build an address that silently fails to match a parsed
    /// one (the same class of bug the newtype exists to prevent).
    pub fn new(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        let raw_name = name.into();
        let canonical = crate::utils::canonicalize_map_key_address(&raw_name);
        Self {
            provider: provider.into(),
            resource_type: resource_type.into(),
            name: crate::resource::ResourceName::from_string(canonical),
        }
    }

    /// Borrow the address's name as `&str`.
    pub fn name_str(&self) -> &str {
        self.name.as_str()
    }

    /// Returns the display type including provider prefix, matching
    /// [`ResourceId::display_type`] so plan/error output reads the
    /// same for both routing-agnostic and routed addresses.
    pub fn display_type(&self) -> String {
        if self.provider.is_empty() {
            self.resource_type.clone()
        } else {
            format!("{}.{}", self.provider, self.resource_type)
        }
    }

    /// Last-resort lift to a [`ResourceId`] with no routing.
    ///
    /// Use this **only** when the address resolved against no plan or
    /// state entry (e.g. the user wrote `import { to = X 'addr' }` for
    /// a resource that does not exist anywhere). The resulting
    /// `ResourceId` carries `provider_instance = None`, which is the
    /// best the type system can do when there is no row to inherit
    /// routing from.
    ///
    /// Normal resolution paths must instead inherit routing from the
    /// matched plan Create or state row — they should never call this
    /// method as a shortcut. Doing so re-introduces the carina#3324
    /// bug class.
    pub fn to_unrouted_resource_id(&self) -> crate::resource::ResourceId {
        crate::resource::ResourceId::with_provider(
            &self.provider,
            &self.resource_type,
            self.name_str(),
            None,
        )
    }
}

impl std::fmt::Display for StateBlockAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.provider.is_empty() {
            write!(f, "{}.{}", self.resource_type, self.name)
        } else {
            write!(f, "{}.{}.{}", self.provider, self.resource_type, self.name)
        }
    }
}

/// State manipulation block (import, removed, moved)
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StateBlock {
    /// Import existing infrastructure into Carina management
    Import {
        /// Target resource address (routing-agnostic — see
        /// [`StateBlockAddress`]).
        to: StateBlockAddress,
        /// Cloud provider identifier (e.g., `"vpc-0abc123def456"`).
        ///
        /// Carried as a [`Value`] (not `String`) so a `"${X.attr}|..."`
        /// interpolation referencing a deferred upstream-state value
        /// stays a `Value::Deferred(DeferredValue::Interpolation)` from
        /// parse through plan-time resolution and display. The pre-#3329
        /// `String` shape silently dropped `${...}` segments at parse
        /// time and presented a partially-substituted literal as if it
        /// were a real cloud identifier. See carina#3329.
        id: Value,
    },
    /// Remove a resource from state without destroying it
    Removed {
        /// Resource address to remove from state (routing-agnostic).
        from: StateBlockAddress,
    },
    /// Rename/move a resource in state without destroy/recreate
    Moved {
        /// Old resource address (routing-agnostic).
        from: StateBlockAddress,
        /// New resource address (routing-agnostic).
        to: StateBlockAddress,
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
    /// Non-literal well-known attributes (e.g. `default_tags = some_let.field`)
    /// drained and validated by the post-resolver finalize step. Invariant:
    /// empty after finalization. In-memory transit only — never serialized.
    #[serde(skip)]
    pub unresolved_attributes: IndexMap<String, Value>,
    /// `let` binding name when this entry was declared as a named instance
    /// via `let <name> = provider <kind> { ... }`. `None` for the default
    /// instance produced by a top-level `provider <kind> { ... }` block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    /// True when this entry is the kind's default instance (sourced from a
    /// top-level `provider <kind> { ... }` block). False for named instances.
    /// Resources without an explicit provider directive resolve to the
    /// kind's default instance.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub is_default: bool,
}

fn default_true() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
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

/// Typed `until` predicate captured at parse time. MVP supports the
/// `<binding>.<attr-path> == <value>` shape; future operators
/// (`!=`, `&&`/`||`, comparisons, `in`) will grow new variants here
/// without breaking existing fields.
///
/// `lhs_segments` is the dotted path under the target binding —
/// `[target, attr]` for `cert.status`, `[target, parent, attr]` for
/// `cert.renewal_summary.renewal_status`. Always non-empty and the
/// first segment is the target binding name (enforced by the parser).
///
/// `rhs` is the literal value to compare against, captured as a
/// `Value` so namespaced enums (`aws.acm.Certificate.Status.Issued`),
/// string literals, integers, booleans, and durations all flow into
/// the same predicate type.
#[derive(Debug, Clone)]
pub struct UntilPredicateAst {
    pub lhs_segments: Vec<String>,
    pub rhs: crate::resource::Value,
}

/// A binding identifier (a `let`/`wait`/`depends_on` name such as
/// `cert_issued` or its instance-prefixed form `r.cert_issued`).
///
/// This newtype exists so a binding name cannot be confused with an
/// arbitrary `String` at a binding-expecting position, and so the
/// instance-prefix operation is typed as `BindingName -> BindingName`
/// (see `module_resolver::apply_instance_prefix`). It deliberately does
/// **not** encode prefix state (Raw vs Prefixed): the wait-binding list
/// mixes top-level (never-prefixed) and module-derived (prefixed)
/// entries in one `Vec`, so a state-in-the-type distinction is not
/// expressible here — that larger data-flow reshape is tracked
/// separately (carina#3066).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BindingName(String);

impl BindingName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::ops::Deref for BindingName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BindingName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for BindingName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for BindingName {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Compare a `BindingName` directly against a string slice so existing
/// `wb.target == some_str` / lookup sites don't need `.as_str()`
/// ceremony. The newtype's value is preventing *accidental* String
/// substitution at construction/parameter boundaries, not forbidding
/// equality checks.
impl PartialEq<str> for BindingName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for BindingName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<BindingName> for str {
    fn eq(&self, other: &BindingName) -> bool {
        self == other.0
    }
}

impl PartialEq<BindingName> for &str {
    fn eq(&self, other: &BindingName) -> bool {
        *self == other.0
    }
}

impl PartialEq<String> for BindingName {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

impl PartialEq<BindingName> for String {
    fn eq(&self, other: &BindingName) -> bool {
        self == &other.0
    }
}

/// A `wait <target> { ... }` declaration captured during parse.
///
/// Carries the parsed surface form of the `until` predicate so plan
/// display can echo the user-authored expression verbatim, plus the
/// structured `until_predicate` for the differ / executor to lower
/// into `WaitPredicate`. `timeout_secs` is normalised to seconds
/// because `Duration` from carina#2824 already canonicalises that way.
///
/// See `notes/specs/2026-05-09-wait-construct-design.md`.
#[derive(Debug, Clone)]
pub struct WaitBinding {
    /// The wait's binding name (e.g. `cert_issued`).
    pub binding: BindingName,
    /// Identifier of the target resource binding (e.g. `cert`).
    pub target: BindingName,
    /// Surface form of the `until` expression as the user wrote it
    /// (e.g. `"cert.status == aws.acm.Certificate.Status.Issued"`).
    pub until_raw: String,
    /// Structured predicate for the differ / executor to consume.
    pub until_predicate: UntilPredicateAst,
    /// Optional user override of the wait timeout, in whole seconds. When
    /// `None` the differ falls back to the target schema's default.
    pub timeout_secs: Option<u64>,
    /// Additional ordering edges declared via `depends_on = [...]`. The
    /// ordering machinery itself is shared with the per-resource
    /// `directives.depends_on` (carina#2823).
    pub depends_on: Vec<BindingName>,
    /// Source line of the `wait` keyword. Used by diagnostics.
    pub line: usize,
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
    fn type_expr_opt(&self) -> Option<&TypeExpr> {
        Some(&self.type_expr)
    }
    fn type_expr_opt_mut(&mut self) -> Option<&mut TypeExpr> {
        Some(&mut self.type_expr)
    }
}

/// Parse result, generic over the export-parameter shape.
///
/// Two phases share this struct via aliases (see [`ParsedFile`] and
/// [`InferredFile`]): the parser produces `File<ParsedExportParam>`
/// where `type_expr` is `Option<TypeExpr>`; the loader runs
/// `apply_inference` and yields `File<InferredExportParam>` where every
/// export's `type_expr` is bare. Every other field is identical between
/// phases. See `notes/specs/2026-05-03-typeexpr-stage2-design.md`.
#[derive(Debug, Clone)]
pub struct File<E> {
    pub providers: Vec<ProviderConfig>,
    /// Top-level managed infrastructure resources.
    pub resources: Vec<Resource>,
    /// Read-only data-source resources (`read`-keyword resources).
    pub data_sources: Vec<DataSource>,
    /// Virtual resources synthesized by module-call expansion.
    pub compositions: Vec<Composition>,
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
    /// `wait` bindings declared via `let <name> = wait <target> { ... }`.
    pub wait_bindings: Vec<WaitBinding>,
    /// Require blocks (cross-argument constraints)
    pub requires: Vec<RequireBlock>,
    /// Binding names that are structurally required (if/for/read expressions)
    /// and should not trigger unused-binding warnings.
    pub structural_bindings: HashSet<String>,
    /// Non-fatal warnings collected during parsing.
    pub warnings: Vec<ParseWarning>,
    /// For-expressions whose iterables are unresolved; displayed as deferred in plan.
    pub deferred_for_expressions: Vec<DeferredForExpression>,
    /// Plan-scoped lineage of leaf nodes back to the composition call
    /// sites that produced them (#3306).
    ///
    /// Populated by `ModuleResolver::expand_module_call`: every leaf
    /// resource added to the expanded `File` records its originating
    /// call-site chain (outermost first). Leaves declared at the DSL
    /// root are absent from the map.
    ///
    /// Not serialized to state — the trace is plan-scoped and rebuilt
    /// from DSL on every parse. (`File` itself does not derive Serde,
    /// so no annotation is required.)
    pub expansion_trace: crate::resource::ExpansionTrace,
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
            data_sources: Vec::new(),
            compositions: Vec::new(),
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
            wait_bindings: Vec::new(),
            requires: Vec::new(),
            structural_bindings: HashSet::new(),
            warnings: Vec::new(),
            deferred_for_expressions: Vec::new(),
            expansion_trace: crate::resource::ExpansionTrace::new(),
        }
    }
}

impl<E> File<E> {
    /// Transform only the export-param phase (`File<E>` → `File<B>`),
    /// applying `f` to the export params and moving every other field
    /// through unchanged.
    ///
    /// This is the **single** place the "every non-export field passes
    /// a phase change untouched" knowledge lives. The struct is
    /// **destructured exhaustively** — the carina#3126 / carina#3061
    /// compile-time forcing function for the *phase axis*: a new
    /// `File<E>` field cannot compile until it is moved through here
    /// too. Both [`relabel_export_phase`](crate::config_loader) (module
    /// contribution → caller phase, export params asserted empty) and
    /// `apply_inference` (parser → inferred, export params type-inferred)
    /// delegate here instead of hand-listing every field each.
    pub fn map_export_params<B>(self, f: impl FnOnce(Vec<E>) -> Vec<B>) -> File<B> {
        let File {
            providers,
            resources,
            data_sources,
            compositions,
            variables,
            uses,
            module_calls,
            arguments,
            attribute_params,
            export_params,
            backend,
            state_blocks,
            user_functions,
            upstream_states,
            wait_bindings,
            requires,
            structural_bindings,
            warnings,
            deferred_for_expressions,
            expansion_trace,
        } = self;

        File {
            providers,
            resources,
            data_sources,
            compositions,
            variables,
            uses,
            module_calls,
            arguments,
            attribute_params,
            export_params: f(export_params),
            backend,
            state_blocks,
            user_functions,
            upstream_states,
            wait_bindings,
            requires,
            structural_bindings,
            warnings,
            deferred_for_expressions,
            expansion_trace,
        }
    }

    /// Iterate every resource reachable from the parsed file — the
    /// top-level managed resources, composition resources, data sources, and
    /// the `template_resource` of each deferred for-expression — each
    /// wrapped in a typed [`ResourceRef`].
    ///
    /// Per-attribute checkers (type, enum, required, ref validity, etc.)
    /// should prefer this over `self.resources.iter()` so they stay in sync
    /// with for-body code. See
    /// `notes/specs/2026-04-19-unify-resource-walk-design.md` for the
    /// rationale.
    ///
    /// **carina#3181 PR C:** `self.resources` is now managed-only — the
    /// parser, module expander, and deferred-for expansion write each
    /// resource into exactly one of `resources` / `data_sources` /
    /// `compositions`. The iterator chains the three typed slices
    /// plus the deferred for-expression templates.
    pub fn iter_all_resources(&self) -> impl Iterator<Item = ResourceRef<'_>> {
        self.resources
            .iter()
            .map(ResourceRef::Resource)
            .chain(self.compositions.iter().map(ResourceRef::Composition))
            .chain(self.data_sources.iter().map(ResourceRef::DataSource))
            .chain(
                self.deferred_for_expressions
                    .iter()
                    .map(|d| ResourceRef::Deferred {
                        resource: &d.template_resource,
                        deferred: d,
                    }),
            )
    }

    /// Iterate the top-level resources — managed, composition, and data
    /// source — as typed [`ResourceRef`]s, **excluding** deferred
    /// for-expression templates.
    ///
    /// Use this over [`iter_all_resources`](Self::iter_all_resources)
    /// when a consumer needs only the concrete top-level declarations
    /// (binding-name collection, dependency sorting) and a for-body
    /// template would be a spurious entry.
    pub fn iter_top_level_resources(&self) -> impl Iterator<Item = ResourceRef<'_>> {
        self.resources
            .iter()
            .map(ResourceRef::Resource)
            .chain(self.compositions.iter().map(ResourceRef::Composition))
            .chain(self.data_sources.iter().map(ResourceRef::DataSource))
    }

    /// Every non-variable binding name declared in the file.
    ///
    /// This intentionally excludes `variables`: parser-side variables
    /// include both plain value `let`s and placeholder entries for
    /// structural `let`s. Callers that need the complete in-scope set
    /// should union this with `variables`; callers that need to classify
    /// plain value lets can use this as the exclusion set.
    pub(crate) fn structural_binding_names(&self) -> HashSet<&str> {
        let mut names = HashSet::new();
        names.extend(self.iter_top_level_resources().filter_map(|r| r.binding()));
        names.extend(self.arguments.iter().map(|a| a.name.as_str()));
        names.extend(
            self.module_calls
                .iter()
                .filter_map(|c| c.binding_name.as_deref()),
        );
        names.extend(self.upstream_states.iter().map(|u| u.binding.as_str()));
        names.extend(self.wait_bindings.iter().map(|w| w.binding.as_str()));
        names.extend(self.uses.iter().map(|u| u.alias.as_str()));
        names.extend(self.user_functions.keys().map(String::as_str));
        names.extend(self.providers.iter().filter_map(|p| p.binding.as_deref()));
        names.extend(self.structural_bindings.iter().map(String::as_str));
        names
    }

    /// Consume `self` and yield the top-level nodes as owned
    /// [`GraphNode`]s.
    ///
    /// The owned counterpart to
    /// [`iter_top_level_resources`](Self::iter_top_level_resources):
    /// when a caller needs to take ownership of the three typed slices
    /// and treat them uniformly (post-expansion plan-engine paths,
    /// ownership transfer into intermediate representations), this
    /// returns a single `Iterator<Item = GraphNode>` instead of three
    /// parallel `Vec`s. Deferred for-expression templates are excluded
    /// for the same reason as the borrowing iterator above.
    pub fn into_graph_nodes(self) -> impl Iterator<Item = GraphNode> {
        self.resources
            .into_iter()
            .map(GraphNode::from)
            .chain(self.compositions.into_iter().map(GraphNode::from))
            .chain(self.data_sources.into_iter().map(GraphNode::from))
    }

    /// Consume `self` and yield the leaf nodes as owned
    /// [`LeafNode`]s — i.e. resources and data sources only.
    ///
    /// `LeafNode` is the subset of [`GraphNode`] that has no
    /// `Composition` variant by construction, so a downstream caller
    /// that takes `Vec<LeafNode>` (the post-expansion view) cannot
    /// be handed a composition through the type system. This is the
    /// type-level counterpart to
    /// [`into_graph_nodes`](Self::into_graph_nodes), and is the
    /// boundary the differ / executor pipeline crosses at the end of
    /// composition expansion.
    ///
    /// Compositions are dropped from the output; the originating
    /// `Composition` → leaf lineage will be carried separately by the
    /// `ExpansionTrace` once #3295 introduces it.
    pub fn into_leaf_nodes(self) -> impl Iterator<Item = LeafNode> {
        self.resources
            .into_iter()
            .map(LeafNode::from)
            .chain(self.data_sources.into_iter().map(LeafNode::from))
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
                && matches!(r.get_attr(attr_name), Some(Value::Concrete(ConcreteValue::String(n))) if n == attr_value)
        })
    }

    /// Print all collected warnings to stderr.
    ///
    /// Returns `true` iff at least one warning line was printed. Callers that
    /// interleave this with indicatif progress output use the return value to
    /// know the terminal's last line is now a newline-terminated `⚠` line
    /// (not an open spinner bar) — see
    /// `carina-cli`'s `finish_refresh_bar_region`.
    pub fn print_warnings(&self) -> bool {
        for w in &self.warnings {
            let location = match &w.file {
                Some(f) => format!("{}:{}", f, w.line),
                None => format!("line {}", w.line),
            };
            eprintln!("  ⚠ {}: {}", location, w.message);
        }
        !self.warnings.is_empty()
    }

    /// Expand deferred for-expressions against the resolved binding view.
    ///
    /// For each deferred for-expression whose iterable can now be resolved
    /// from `bindings`, expand the template into concrete resources and
    /// add them to `self.resources`. Resolved entries are removed from
    /// `deferred_for_expressions`; unresolved ones remain (with their
    /// warning preserved).
    ///
    /// `bindings` is an [`IterableBindings`]: every binding an iterable
    /// may reference — same-config `let` resources (post-refresh),
    /// `upstream_state` data, and `wait` aliases — merged into one view.
    /// Taking the typed view rather than the raw upstream-only map is the
    /// carina#3132 fix: a same-config `let cert` read iterable
    /// (`for _, opt in cert.domain_validation_options`) is in scope here
    /// only because the caller projects the post-refresh
    /// [`crate::binding_index::ResolvedBindings`] in.
    pub fn expand_deferred_for_expressions(&mut self, bindings: &IterableBindings) {
        let mut expanded_resources = Vec::new();
        let mut resolved_indices = Vec::new();
        // Indices where the iterable resolved but had the wrong shape. These
        // entries stay deferred (user must fix), but their parse-time "not yet
        // available" warning is replaced by the more specific shape-mismatch
        // warning collected in `new_warnings`.
        let mut mismatched_indices: Vec<usize> = Vec::new();
        let mut new_warnings: Vec<ParseWarning> = Vec::new();

        for (idx, deferred) in self.deferred_for_expressions.iter().enumerate() {
            // Look up the iterable value in the merged binding view
            let iterable = bindings
                .get(&deferred.iterable_binding)
                .and_then(|attrs| attrs.get(&deferred.iterable_attr));

            let Some(iterable_value) = iterable else {
                continue;
            };

            match (&deferred.binding, iterable_value) {
                // Simple binding: only the value var is bound
                (ForBinding::Simple(_), Value::Concrete(ConcreteValue::List(items))) => {
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
                (ForBinding::Indexed(_, _), Value::Concrete(ConcreteValue::List(items))) => {
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
                (ForBinding::Map(_, _), Value::Concrete(ConcreteValue::Map(map))) => {
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
                (ForBinding::Map(_, _), Value::Concrete(ConcreteValue::List(_))) => {
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
                (ForBinding::Simple(_), Value::Concrete(ConcreteValue::Map(_)))
                | (ForBinding::Indexed(_, _), Value::Concrete(ConcreteValue::Map(_))) => {
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

        // carina#3181: a deferred for-expression's `template_resource` is
        // a `Resource`, so every expansion of it is managed — they
        // go straight into the managed `resources` slice. (A `read` /
        // composition for-body never reaches the deferred path.)
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
/// Replaces `Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue))` with `value`. If
/// `index` is supplied (indexed-binding expansion), replaces
/// `Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex))` with the integer index. If
/// `key` is supplied (map-binding expansion), replaces
/// `Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey))` with the key string. Recurses
/// into all compound Value variants so placeholders nested inside
/// interpolations / function calls / secrets are reached.
///
/// `UnknownReason::UpstreamRef` is the upstream-attribute marker, not a
/// for-binding placeholder, so it is preserved unchanged.
pub(super) fn substitute_placeholder(
    v: &mut Value,
    index: Option<i64>,
    key: Option<&str>,
    value: &Value,
) {
    match v {
        Value::Deferred(DeferredValue::Unknown(reason)) => match reason.clone() {
            UnknownReason::ForValue => {
                *v = value.clone();
            }
            UnknownReason::ForValuePath { path } => {
                // carina#3136: a field access on the loop variable
                // (`opt.resource_record.name`). The element is now known
                // (`value`); re-navigate it along the remembered path via
                // the *same* navigator the parse-time resolved case uses
                // (single source of truth). If it does not navigate (the
                // element genuinely lacks that path), leave the placeholder
                // so the existing "unresolved" surfacing applies rather
                // than silently substituting a wrong value.
                if let Some(resolved) = crate::resource::navigate_value_path(value, &path) {
                    *v = resolved;
                }
            }
            UnknownReason::ForKey => {
                if let Some(k) = key {
                    *v = Value::Concrete(ConcreteValue::String(k.to_string()));
                }
            }
            UnknownReason::ForIndex => {
                if let Some(i) = index {
                    *v = Value::Concrete(ConcreteValue::Int(i));
                }
            }
            // Upstream and empty-interpolation unknowns are not for-expansion placeholders.
            UnknownReason::UpstreamRef { .. }
            | UnknownReason::UpstreamBareRef { .. }
            | UnknownReason::EmptyInterpolation => {}
            // Function placeholders are substituted by user-function evaluation, not for expansion.
            UnknownReason::FnParam { .. } | UnknownReason::FnLocal { .. } => {}
        },
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items.iter_mut() {
                substitute_placeholder(item, index, key, value);
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for val in map.values_mut() {
                substitute_placeholder(val, index, key, value);
            }
        }
        Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
            for arg in args.iter_mut() {
                substitute_placeholder(arg, index, key, value);
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts.iter_mut() {
                if let crate::resource::InterpolationPart::Expr(inner) = part {
                    substitute_placeholder(inner, index, key, value);
                }
            }
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            substitute_placeholder(inner, index, key, value);
        }
        _ => {}
    }
}

#[cfg(test)]
mod substitute_placeholder_tests {
    use super::*;
    use crate::resource::{
        AccessPath, ConcreteValue, DeferredValue, InterpolationPart, ResourceId, Signature,
        UnknownReason, Value,
    };
    use std::collections::BTreeSet;

    #[test]
    fn structural_binding_names_covers_every_non_variable_kind() {
        let mut parsed = ParsedFile::default();

        let mut resource = Resource::new("mock.compute.Instance", "resource_node");
        resource.binding = Some("resource_name".to_string());
        parsed.resources.push(resource); // allow: direct — fixture test inspection

        let mut data_source = DataSource::new("mock.compute.Instance", "data_node");
        data_source.binding = Some("data_source_name".to_string());
        parsed.data_sources.push(data_source);

        parsed.compositions.push(Composition {
            id: ResourceId::new("_virtual", "composition_node"),
            signature: Signature {
                arguments: IndexMap::new(),
                attributes: IndexMap::new(),
            },
            binding: Some("composition_name".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_name: "module_name".to_string(),
            instance: "composition_name".to_string(),
            quoted_string_attrs: HashSet::new(),
        });

        parsed.arguments.push(ArgumentParameter {
            name: "argument_name".to_string(),
            type_expr: TypeExpr::String,
            default: None,
            description: None,
            validations: Vec::new(),
        });
        parsed.module_calls.push(ModuleCall {
            module_name: "module".to_string(),
            binding_name: Some("module_call_name".to_string()),
            arguments: HashMap::new(),
        });
        parsed.upstream_states.push(UpstreamState {
            binding: "upstream_name".to_string(),
            source: "upstream".into(),
        });
        parsed.wait_bindings.push(WaitBinding {
            binding: BindingName::from("wait_name"),
            target: BindingName::from("resource_name"),
            until_raw: "resource_name.status == 'ready'".to_string(),
            until_predicate: UntilPredicateAst {
                lhs_segments: vec!["resource_name".to_string(), "status".to_string()],
                rhs: Value::Concrete(ConcreteValue::String("ready".to_string())),
            },
            timeout_secs: None,
            depends_on: Vec::new(),
            line: 1,
        });
        parsed.uses.push(UseStatement {
            path: "./module".to_string(),
            alias: "use_alias".to_string(),
        });
        parsed.user_functions.insert(
            "function_name".to_string(),
            UserFunction {
                name: "function_name".to_string(),
                params: Vec::new(),
                return_type: None,
                local_lets: Vec::new(),
                body: UserFunctionBody(Value::Concrete(ConcreteValue::Int(1))),
            },
        );
        parsed.providers.push(ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: Some("provider_name".to_string()),
            is_default: false,
        });
        parsed
            .structural_bindings
            .insert("structural_name".to_string());
        parsed.variables.insert(
            "plain_value_name".to_string(),
            Value::Concrete(ConcreteValue::String("literal".to_string())),
        );

        let names = parsed.structural_binding_names();
        for expected in [
            "resource_name",
            "data_source_name",
            "composition_name",
            "argument_name",
            "module_call_name",
            "upstream_name",
            "wait_name",
            "use_alias",
            "function_name",
            "provider_name",
            "structural_name",
        ] {
            assert!(
                names.contains(expected),
                "structural_binding_names should include {expected}; got {names:?}"
            );
        }
        assert!(
            !names.contains("plain_value_name"),
            "plain value lets must be excluded from structural binding names"
        );
    }

    #[test]
    fn replaces_for_value_with_bound_value() {
        let mut v = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue));
        substitute_placeholder(
            &mut v,
            None,
            None,
            &Value::Concrete(ConcreteValue::String("acct-1".into())),
        );
        assert_eq!(v, Value::Concrete(ConcreteValue::String("acct-1".into())));
    }

    #[test]
    fn replaces_for_key_with_key_string() {
        let mut v = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey));
        substitute_placeholder(
            &mut v,
            None,
            Some("east"),
            &Value::Concrete(ConcreteValue::String("ignored".into())),
        );
        assert_eq!(v, Value::Concrete(ConcreteValue::String("east".into())));
    }

    #[test]
    fn replaces_for_index_with_integer_index() {
        let mut v = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForIndex));
        substitute_placeholder(
            &mut v,
            Some(7),
            None,
            &Value::Concrete(ConcreteValue::String("ignored".into())),
        );
        assert_eq!(v, Value::Concrete(ConcreteValue::Int(7)));
    }

    #[test]
    fn leaves_upstream_ref_unchanged() {
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
        let mut v = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
            path: path.clone(),
        }));
        substitute_placeholder(
            &mut v,
            Some(0),
            Some("k"),
            &Value::Concrete(ConcreteValue::String("anything".into())),
        );
        // `Value::Deferred(DeferredValue::Unknown)` never compares equal (see `impl PartialEq for Value`),
        // so destructure to verify the path survives.
        match v {
            Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef { path: p })) => {
                assert_eq!(p, path)
            }
            other => panic!("UpstreamRef must pass through unchanged, got {:?}", other),
        }
    }

    #[test]
    fn recurses_into_compound_values() {
        let mut v = Value::Concrete(ConcreteValue::List(vec![
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
            Value::Concrete(ConcreteValue::Map({
                let mut m = indexmap::IndexMap::new();
                m.insert(
                    "k".into(),
                    Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
                );
                m
            })),
            Value::Deferred(DeferredValue::Interpolation(vec![InterpolationPart::Expr(
                Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
            )])),
        ]));
        substitute_placeholder(
            &mut v,
            None,
            None,
            &Value::Concrete(ConcreteValue::String("X".into())),
        );
        match &v {
            Value::Concrete(ConcreteValue::List(items)) => {
                assert_eq!(items[0], Value::Concrete(ConcreteValue::String("X".into())));
                match &items[1] {
                    Value::Concrete(ConcreteValue::Map(m)) => {
                        assert_eq!(m["k"], Value::Concrete(ConcreteValue::String("X".into())));
                    }
                    other => panic!("expected Map, got {:?}", other),
                }
                match &items[2] {
                    Value::Deferred(DeferredValue::Interpolation(parts)) => match &parts[0] {
                        InterpolationPart::Expr(inner) => {
                            assert_eq!(*inner, Value::Concrete(ConcreteValue::String("X".into())));
                        }
                        _ => panic!("expected Expr part"),
                    },
                    other => panic!("expected Interpolation, got {:?}", other),
                }
            }
            other => panic!("expected List, got {:?}", other),
        }
    }

    #[test]
    fn legacy_string_sentinel_is_no_longer_recognised() {
        // Only `Value::Deferred(DeferredValue::Unknown(For*))` is substituted. A bare string that
        // happens to match the historical sentinel must be left alone.
        let mut v = Value::Concrete(ConcreteValue::String("(known after upstream apply)".into()));
        substitute_placeholder(
            &mut v,
            None,
            None,
            &Value::Concrete(ConcreteValue::String("X".into())),
        );
        assert_eq!(
            v,
            Value::Concrete(ConcreteValue::String("(known after upstream apply)".into()))
        );
    }
}
