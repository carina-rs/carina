//! Validation utilities for resources and modules

pub mod deferred_populate;
pub mod depends_on;
pub mod wait;

use std::collections::HashSet;

use indexmap::IndexMap;

use crate::binding_index::{BindingIndex, RefType, RefTypeError, ResolvedRefType};
use crate::deps::collect_dependencies;
use crate::parser::{
    ModuleCall, ProviderContext, ResourceRef, ResourceTypePath, TypeExpr, validate_custom_type,
};
use crate::provider::ProviderFactory;
use crate::resource::{AccessPath, CompositionCall, ConcreteValue, DeferredValue, Value};
use crate::schema::{AttributeType, SchemaRegistry, Shape, TypeIdentity};

/// Lift a module-boundary [`TypeExpr`] into the schema type system.
///
/// The returned type is definition-free and can therefore be paired with
/// [`crate::schema::TypeInSchema::schemaless`]. Callers must still choose the
/// direction of assignment explicitly: a declared input/output type used as a
/// sink belongs on the right-hand side of `source.is_assignable_to(sink)`,
/// while a declared upstream type used as a source belongs on the left.
///
/// Resource-handle annotations ([`TypeExpr::Ref`]) deliberately remain
/// unchecked because a resource handle is not representable as an
/// [`AttributeType`] yet (carina#3803). String literals, unresolved dotted
/// paths, and failed-inference sentinels also remain unchecked rather than
/// being widened to `String`; inventing a wider type would make identity sinks
/// accept values without evidence.
pub fn lift_type_expr(type_expr: &TypeExpr) -> Option<AttributeType> {
    use crate::schema::StructField;

    match type_expr {
        TypeExpr::String => Some(AttributeType::string()),
        TypeExpr::Bool => Some(AttributeType::bool()),
        TypeExpr::Int => Some(AttributeType::int()),
        TypeExpr::Float => Some(AttributeType::float()),
        TypeExpr::Duration => Some(AttributeType::duration()),
        TypeExpr::Simple(name) => Some(AttributeType::refined_string(
            Some(TypeIdentity::bare(crate::parser::snake_to_pascal(name))),
            None,
            None,
            None,
        )),
        TypeExpr::SchemaType {
            provider,
            path,
            type_name,
        } => Some(AttributeType::refined_string(
            Some(TypeIdentity::from_schema_type(provider, path, type_name)),
            None,
            None,
            None,
        )),
        TypeExpr::List(inner) => lift_type_expr(inner).map(AttributeType::list),
        TypeExpr::Map(inner) => lift_type_expr(inner).map(AttributeType::map),
        TypeExpr::Struct { fields } => fields
            .iter()
            .map(|(name, field_type)| {
                lift_type_expr(field_type)
                    .map(|field_type| StructField::new(name.clone(), field_type))
            })
            .collect::<Option<Vec<_>>>()
            .map(|fields| AttributeType::struct_("anonymous", fields)),
        TypeExpr::Union(members) => members
            .iter()
            .map(lift_type_expr)
            .collect::<Option<Vec<_>>>()
            .filter(|members| !members.is_empty())
            .map(AttributeType::union),
        // Ref denotes a resource handle, not an Id/Arn projection. See #3803.
        TypeExpr::Ref(_)
        // These forms have no conservative schema-level representation.
        | TypeExpr::StringLiteral(_)
        | TypeExpr::DottedUnresolved(_)
        | TypeExpr::Unknown => None,
    }
}

static STRING_REF_SINK: TypeExpr = TypeExpr::String;

/// The declared type at one position in a [`Value`] tree.
///
/// Module boundaries use `TypeExpr`; provider resource attributes use
/// `AttributeType` plus their definition map. `String` represents the inside
/// of an interpolation, whose expression parts are rendered to text regardless
/// of the outer container.
#[derive(Clone, Copy)]
pub(crate) enum RefSink<'a> {
    TypeExpr(&'a TypeExpr),
    AttributeType {
        attr_type: &'a AttributeType,
        defs: &'a std::collections::BTreeMap<String, AttributeType>,
    },
    String,
}

impl<'a> RefSink<'a> {
    fn as_type_expr(self) -> Option<&'a TypeExpr> {
        match self {
            Self::TypeExpr(type_expr) => Some(type_expr),
            Self::String => Some(&STRING_REF_SINK),
            Self::AttributeType { .. } => None,
        }
    }

    fn list_element(self) -> Option<Self> {
        match self {
            Self::TypeExpr(TypeExpr::List(inner)) => Some(Self::TypeExpr(inner)),
            Self::TypeExpr(TypeExpr::Union(members)) => {
                exactly_one(members.iter().filter_map(|member| match member {
                    TypeExpr::List(inner) => Some(inner.as_ref()),
                    _ => None,
                }))
                .map(Self::TypeExpr)
            }
            Self::AttributeType { attr_type, defs } => match attr_type.shape_with_defs(defs) {
                Shape::List {
                    element_type: inner,
                    ..
                } => Some(Self::AttributeType {
                    attr_type: inner,
                    defs,
                }),
                Shape::Union => {
                    unique_union_list_element(attr_type, defs).map(|inner| Self::AttributeType {
                        attr_type: inner,
                        defs,
                    })
                }
                _ => None,
            },
            Self::TypeExpr(_) | Self::String => None,
        }
    }

    fn map_entry(self, key: &str) -> Option<Self> {
        match self {
            Self::TypeExpr(TypeExpr::Map(inner)) => Some(Self::TypeExpr(inner)),
            Self::TypeExpr(TypeExpr::Struct { fields }) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, field_type)| Self::TypeExpr(field_type)),
            Self::TypeExpr(TypeExpr::Union(members)) => exactly_one(
                members
                    .iter()
                    .filter(|member| matches!(member, TypeExpr::Map(_) | TypeExpr::Struct { .. })),
            )
            .and_then(|member| Self::TypeExpr(member).map_entry(key)),
            Self::AttributeType { attr_type, defs } => match attr_type.shape_with_defs(defs) {
                Shape::Map { value: inner, .. } => Some(Self::AttributeType {
                    attr_type: inner,
                    defs,
                }),
                Shape::Struct { .. } => {
                    let fields = crate::schema::struct_fields_with_defs(attr_type, defs)
                        .expect("Shape::Struct must expose struct fields internally");
                    let accepted = crate::schema::build_accepted_field_map(fields);
                    accepted.get(key).map(|field| Self::AttributeType {
                        attr_type: &field.field_type,
                        defs,
                    })
                }
                Shape::Union => unique_union_map_member(attr_type, defs).and_then(|member| {
                    Self::AttributeType {
                        attr_type: member,
                        defs,
                    }
                    .map_entry(key)
                }),
                _ => None,
            },
            Self::TypeExpr(_) | Self::String => None,
        }
    }
}

fn exactly_one<T>(mut values: impl Iterator<Item = T>) -> Option<T> {
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn unique_union_list_element<'a>(
    attr_type: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
) -> Option<&'a AttributeType> {
    let members = crate::schema::union_members_with_defs(attr_type, defs)?;
    exactly_one(members.iter().flatten().filter_map(|member| {
        match member.as_attr().shape_with_defs(defs) {
            Shape::List { element_type, .. } => Some(element_type),
            _ => None,
        }
    }))
}

fn unique_union_map_member<'a>(
    attr_type: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
) -> Option<&'a AttributeType> {
    let members = crate::schema::union_members_with_defs(attr_type, defs)?;
    exactly_one(members.iter().flatten().filter_map(|member| {
        matches!(
            member.as_attr().shape_with_defs(defs),
            Shape::Map { .. } | Shape::Struct { .. }
        )
        .then_some(member.as_attr())
    }))
}

/// Visit every resource reference exactly once while carrying the declared
/// sink type for its structural position.
///
/// Lists, maps, and structs descend in lockstep with their declared type.
/// When a value shape has no corresponding declared position, descendants are
/// still visited with `None` so existence diagnostics are never lost. A Union
/// contributes a nested sink only when exactly one member matches the concrete
/// List or Map value shape. Function arguments likewise have no locally
/// available signature and deliberately receive `None`: the function's result
/// sink says nothing sound about its input positions. Secrets preserve their
/// enclosing sink and interpolation expressions use a string sink.
pub(crate) fn visit_refs_with_sink<'a>(
    value: &Value,
    sink: Option<RefSink<'a>>,
    f: &mut impl FnMut(&AccessPath, Option<RefSink<'a>>),
) {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { path }) => f(path, sink),
        Value::Concrete(ConcreteValue::List(items)) => {
            let child_sink = sink.and_then(RefSink::list_element);
            for item in items {
                visit_refs_with_sink(item, child_sink, f);
            }
        }
        Value::Concrete(ConcreteValue::Map(entries)) => {
            for (key, value) in entries {
                let child_sink = sink.and_then(|sink| sink.map_entry(key));
                visit_refs_with_sink(value, child_sink, f);
            }
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            for part in parts {
                if let crate::resource::InterpolationPart::Expr(value) = part {
                    visit_refs_with_sink(value, Some(RefSink::String), f);
                }
            }
        }
        Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
            // The enclosing sink constrains the function result, not its
            // inputs. Parameter signatures are unavailable here, so carrying
            // that sink into an argument would be unsound.
            for argument in args {
                visit_refs_with_sink(argument, None, f);
            }
        }
        Value::Deferred(DeferredValue::Secret(inner)) => visit_refs_with_sink(inner, sink, f),
        Value::Concrete(ConcreteValue::String(_))
        | Value::Concrete(ConcreteValue::EnumIdentifier(_))
        | Value::Concrete(ConcreteValue::CanonicalEnum(_))
        | Value::Concrete(ConcreteValue::Int(_))
        | Value::Concrete(ConcreteValue::Float(_))
        | Value::Concrete(ConcreteValue::Bool(_))
        | Value::Concrete(ConcreteValue::Duration(_))
        | Value::Concrete(ConcreteValue::StringList(_))
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::Unknown(_)) => {}
    }
}

fn check_resource_ref_existence(
    resource_id: &crate::resource::ResourceId,
    ref_path: &crate::resource::AccessPath,
    argument_names: &HashSet<String>,
    bindings: &BindingIndex<'_>,
    all_errors: &mut Vec<String>,
) -> Option<ResolvedRefType> {
    let ref_binding = ref_path.binding();
    let ref_attr = ref_path.attribute();

    // Skip type checking for argument parameter references (resolved at call site)
    if argument_names.contains(ref_binding) {
        return None;
    }

    match bindings.ref_type(ref_path) {
        RefType::Typed(resolved) => Some(resolved),
        RefType::Unchecked => None,
        RefType::UnknownBinding { .. } => {
            all_errors.push(format!(
                "{}: unknown binding '{}' in reference {}.{}",
                resource_id, ref_binding, ref_binding, ref_attr,
            ));
            None
        }
        RefType::UnknownAttribute(error) => {
            all_errors.push(format!("{}: {}", resource_id, error));
            None
        }
    }
}

fn check_nested_resource_ref_existence(
    resource_id: &crate::resource::ResourceId,
    attr_value: &Value,
    argument_names: &HashSet<String>,
    bindings: &BindingIndex<'_>,
    all_errors: &mut Vec<String>,
) {
    attr_value.visit_resource_refs(&mut |ref_path| {
        let _ = check_resource_ref_existence(
            resource_id,
            ref_path,
            argument_names,
            bindings,
            all_errors,
        );
    });
}

#[derive(Debug, Clone)]
pub enum ModuleCallRefErrorKind {
    TypeMismatch { expected: String, actual: String },
    UnknownAttribute(RefTypeError),
}

/// Structured module-call reference error shared by CLI and LSP surfaces.
///
/// `Display` is the stable CLI message. The LSP consumes the named fields and
/// never has to recover source identity by parsing that message.
#[derive(Debug, Clone)]
pub struct ModuleCallRefError {
    pub call: String,
    pub argument: String,
    pub path: AccessPath,
    pub kind: ModuleCallRefErrorKind,
}

impl std::fmt::Display for ModuleCallRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            ModuleCallRefErrorKind::TypeMismatch { expected, actual } => write!(
                f,
                "module call '{}': argument '{}': cannot assign {} to '{}': expected {}, got {} (from {})",
                self.call,
                self.argument,
                actual,
                self.argument,
                expected,
                actual,
                self.path.to_dot_string(),
            ),
            ModuleCallRefErrorKind::UnknownAttribute(error) => {
                write!(f, "module call '{}': {}", self.call, error)
            }
        }
    }
}

impl std::error::Error for ModuleCallRefError {}

/// Validate references carried by module-call argument values against binding
/// surfaces and the called module's declared argument types.
///
/// Unknown bindings and enclosing-module argument names are intentionally
/// ignored here: their resolution belongs to the existing scope/module-call
/// passes. This walk only closes attribute-existence gaps for binding targets
/// that have a statically known schema or module signature.
pub fn validate_module_call_argument_ref_types_with_bindings(
    module_calls: &[ModuleCall],
    imported_modules: &crate::module_resolver::ResolvedModuleSignatures,
    argument_names: &HashSet<String>,
    bindings: &BindingIndex<'_>,
) -> Vec<ModuleCallRefError> {
    let mut errors = Vec::new();

    for call in module_calls {
        let call_name = call
            .binding_name
            .as_deref()
            .unwrap_or(call.module_name.as_str());
        for (argument_name, value) in &call.arguments {
            let declared_sink = imported_modules
                .get(&call.module_name)
                .and_then(|signature| {
                    signature
                        .arguments
                        .iter()
                        .find(|argument| argument.name == *argument_name)
                })
                .map(|argument| &argument.type_expr);

            visit_refs_with_sink(
                value,
                declared_sink.map(RefSink::TypeExpr),
                &mut |path, sink| {
                    if argument_names.contains(path.binding()) {
                        return;
                    }

                    match bindings.ref_type(path) {
                        RefType::Typed(source) => {
                            let Some(sink) = sink
                                .and_then(RefSink::as_type_expr)
                                .and_then(lift_type_expr)
                            else {
                                return;
                            };
                            let source_type = source.type_in_schema();
                            let sink_type = crate::schema::TypeInSchema::schemaless(&sink);
                            if !source_type.is_assignable_to(sink_type) {
                                let source_name = source_type.resolved_type_name();
                                errors.push(ModuleCallRefError {
                                    call: call_name.to_string(),
                                    argument: argument_name.clone(),
                                    path: path.clone(),
                                    kind: ModuleCallRefErrorKind::TypeMismatch {
                                        expected: sink_type.resolved_type_name(),
                                        actual: source_name,
                                    },
                                });
                            }
                        }
                        RefType::UnknownAttribute(error) => {
                            errors.push(ModuleCallRefError {
                                call: call_name.to_string(),
                                argument: argument_name.clone(),
                                path: path.clone(),
                                kind: ModuleCallRefErrorKind::UnknownAttribute(error),
                            });
                        }
                        RefType::Unchecked | RefType::UnknownBinding { .. } => {}
                    }
                },
            );
        }
    }

    errors
}

#[derive(Debug, Clone)]
pub struct AttributeParamRefError {
    pub attribute: String,
    pub path: AccessPath,
    pub kind: ModuleCallRefErrorKind,
}

impl std::fmt::Display for AttributeParamRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            ModuleCallRefErrorKind::TypeMismatch { expected, actual } => write!(
                f,
                "attribute '{}': type mismatch: expected {}, got {} (from {})",
                self.attribute,
                expected,
                actual,
                self.path.to_dot_string(),
            ),
            ModuleCallRefErrorKind::UnknownAttribute(error) => {
                write!(f, "attribute '{}': {}", self.attribute, error)
            }
        }
    }
}

impl std::error::Error for AttributeParamRefError {}

/// A reference-type failure found on one fully expanded composition boundary.
#[derive(Debug, Clone)]
pub enum CompositionRefError {
    ModuleCall(CompositionModuleCallRefError),
    Attribute(CompositionAttributeRefError),
}

/// A call-boundary failure together with structural expansion ancestry used
/// by source-aware consumers such as the LSP.
#[derive(Debug, Clone)]
pub struct CompositionModuleCallRefError {
    pub error: ModuleCallRefError,
    pub call: CompositionCall,
    pub root_call: CompositionCall,
}

impl std::fmt::Display for CompositionModuleCallRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

/// A module-output declaration failure. The module identity remains attached
/// after expansion so repeated instances collapse to one authored error and
/// CLI output can identify the declaration's source.
#[derive(Debug, Clone)]
pub struct CompositionAttributeRefError {
    pub error: AttributeParamRefError,
    pub module_name: String,
    pub module_source: Option<String>,
    pub module_directory: Option<std::path::PathBuf>,
}

impl CompositionAttributeRefError {
    fn module_label(&self) -> &str {
        self.module_source.as_deref().unwrap_or(&self.module_name)
    }

    fn module_identity(&self) -> String {
        self.module_directory
            .as_ref()
            .map(|path| path.display().to_string())
            .or_else(|| self.module_source.clone())
            .unwrap_or_else(|| self.module_name.clone())
    }
}

impl std::fmt::Display for CompositionAttributeRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "module '{}': {}", self.module_label(), self.error)
    }
}

impl std::fmt::Display for CompositionRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModuleCall(error) => error.fmt(f),
            Self::Attribute(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for CompositionRefError {}

fn composition_ref_error_kind(
    path: &AccessPath,
    sink: Option<RefSink<'_>>,
    bindings: &BindingIndex<'_>,
) -> Option<ModuleCallRefErrorKind> {
    match bindings.ref_type(path) {
        RefType::Typed(source) => {
            let sink = sink
                .and_then(RefSink::as_type_expr)
                .and_then(lift_type_expr)?;
            let source_type = source.type_in_schema();
            let sink_type = crate::schema::TypeInSchema::schemaless(&sink);
            (!source_type.is_assignable_to(sink_type)).then(|| {
                ModuleCallRefErrorKind::TypeMismatch {
                    expected: sink_type.resolved_type_name(),
                    actual: source_type.resolved_type_name(),
                }
            })
        }
        RefType::UnknownAttribute(error) => Some(ModuleCallRefErrorKind::UnknownAttribute(error)),
        RefType::Unchecked | RefType::UnknownBinding { .. } => None,
    }
}

/// Validate every fully expanded module-call boundary exactly once.
///
/// Both halves of the boundary live on [`crate::resource::Composition`]:
/// typed call arguments and typed output attributes. The shared
/// [`BindingIndex`] is built only after all nested compositions have been
/// prefixed into the root parse, so unannotated forwarded outputs can inherit
/// their resource-schema type through arbitrarily deep composition chains.
pub fn validate_composition_ref_types_with_bindings(
    compositions: &[crate::resource::Composition],
    bindings: &BindingIndex<'_>,
) -> Vec<CompositionRefError> {
    let mut errors = Vec::new();

    for composition in compositions {
        let call = composition.diagnostic_call();
        let root_call = composition.diagnostic_root_call();
        for (argument_name, argument) in &composition.signature.arguments {
            visit_refs_with_sink(
                argument.value(),
                argument.declared_type().map(RefSink::TypeExpr),
                &mut |path, sink| {
                    let Some(kind) = composition_ref_error_kind(path, sink, bindings) else {
                        return;
                    };
                    errors.push(CompositionRefError::ModuleCall(
                        CompositionModuleCallRefError {
                            error: ModuleCallRefError {
                                call: call.display_label(),
                                argument: argument_name.clone(),
                                path: path.clone(),
                                kind,
                            },
                            call: call.clone(),
                            root_call: root_call.clone(),
                        },
                    ));
                },
            );
        }

        for (attribute_name, attribute) in &composition.signature.attributes {
            let value = attribute.to_value();
            visit_refs_with_sink(
                &value,
                attribute.declared_type().map(RefSink::TypeExpr),
                &mut |path, sink| {
                    let Some(kind) = composition_ref_error_kind(path, sink, bindings) else {
                        return;
                    };
                    errors.push(CompositionRefError::Attribute(
                        CompositionAttributeRefError {
                            error: AttributeParamRefError {
                                attribute: attribute_name.clone(),
                                path: path.clone(),
                                kind,
                            },
                            module_name: call.module_name.clone(),
                            module_source: call.module_source.clone(),
                            module_directory: call.module_directory.clone(),
                        },
                    ));
                },
            );
        }
    }

    let mut seen_calls = HashSet::new();
    let mut seen_attributes = HashSet::new();
    errors.retain(|error| match error {
        CompositionRefError::ModuleCall(error) => {
            seen_calls.insert((error.call.instance.clone(), error.error.to_string()))
        }
        CompositionRefError::Attribute(error) => {
            seen_attributes.insert((error.module_identity(), error.error.attribute.clone()))
        }
    });
    errors
}

/// Validate resources against their schemas.
///
/// Two-sided check: a `read` resource requires a `DataSource` registry entry,
/// and a non-`read` resource requires a `Managed` registry entry. If the
/// wrong-kind entry is present (e.g. `read` against a managed-only type),
/// emit a kind-specific error explaining the mismatch.
pub fn validate_resources<E>(
    parsed: &crate::parser::File<E>,
    registry: &SchemaRegistry,
    known_providers: &HashSet<String>,
    provider_context: &ProviderContext,
) -> Result<(), String> {
    let mut all_errors = Vec::new();
    let lookup = crate::parser::provider_context_lookup(provider_context);

    // Classify per kind via the typed `ResourceRef` arms instead of
    // runtime `is_virtual()` / `is_data_source()` calls (carina#3180 /
    // #3181). compositions are post-apply attribute containers and have no
    // schema to validate against, so they are silently filtered. Managed
    // and data sources route to the same schema-lookup body but render
    // different kind-mismatch diagnostics when the registry entry of the
    // *opposite* kind exists.
    enum ValidatableKind {
        Resource,
        DataSource,
    }
    for rref in parsed.iter_all_resources() {
        // A deferred for-expression template body is always managed —
        // `for` bodies never carry `read` / composition.
        let (kind, schema) = match rref {
            ResourceRef::Composition(_) => continue,
            ResourceRef::DataSource(d) => {
                (ValidatableKind::DataSource, registry.get_for_data_source(d))
            }
            ResourceRef::Resource(m) | ResourceRef::Deferred { resource: m, .. } => {
                (ValidatableKind::Resource, registry.get_for(m))
            }
        };
        let id = rref.id();
        let quoted_string_attrs = rref.quoted_string_attrs();

        match schema {
            Some(schema) => {
                let is_string_literal = |attr: &str| quoted_string_attrs.contains(attr);
                if let Err(errors) = schema.validate_with_origins_and_lookup(
                    &rref.resolved_attributes(),
                    &is_string_literal,
                    &lookup,
                ) {
                    for error in errors {
                        all_errors.push(format!("{}: {}", id, error));
                    }
                }
            }
            None => {
                let provider = id.provider.as_str();
                let resource_type = id.resource_type.as_str();

                // No matching-kind entry. Skip if provider is not loaded —
                // schemas are simply not available, not a configuration error.
                if !provider.is_empty() && !known_providers.contains(provider) {
                    continue;
                }
                let has_managed = registry.has_managed(provider, resource_type);
                let has_data_source = registry.has_data_source(provider, resource_type);
                let kind_label = if provider.is_empty() {
                    resource_type.to_string()
                } else {
                    format!("{}.{}", provider, resource_type)
                };

                match kind {
                    ValidatableKind::DataSource if has_managed => {
                        // `read` used against a managed-only type
                        all_errors.push(format!(
                            "{} is a managed resource, not a data source. Remove the `read` keyword:\n  let <name> = {} {{ }}",
                            kind_label, kind_label
                        ));
                    }
                    ValidatableKind::Resource if has_data_source => {
                        // No `read` against a data-source-only type
                        all_errors.push(format!(
                            "{} is a data source and must be used with the `read` keyword:\n  let <name> = read {} {{ }}",
                            kind_label, kind_label
                        ));
                    }
                    _ => {
                        all_errors.push(format!("Unknown resource type: {}", kind_label));
                    }
                }
            }
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

/// Validate that resource references have compatible types.
///
/// For example, if `ipv4_ipam_pool_id` expects `IpamPoolId` type,
/// a reference like `vpc.vpc_id` (which is `AwsResourceId`) should be an error.
pub fn validate_resource_ref_types<E>(
    parsed: &crate::parser::File<E>,
    registry: &SchemaRegistry,
    argument_names: &HashSet<String>,
    bindings: &BindingIndex<'_>,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    for rref in parsed.iter_all_resources() {
        // A deferred for-expression template body is always managed.
        let schema = match rref {
            ResourceRef::Composition(_) => continue,
            ResourceRef::DataSource(d) => registry.get_for_data_source(d),
            ResourceRef::Resource(m) | ResourceRef::Deferred { resource: m, .. } => {
                registry.get_for(m)
            }
        };
        let Some(schema) = schema else {
            continue;
        };
        let resource_id = rref.id();

        let attrs = rref.attributes();
        for (attr_name, attr_value) in attrs.iter() {
            if attr_name.starts_with('_') {
                continue;
            }

            let Some(attr_schema) = schema.attributes.get(attr_name) else {
                check_nested_resource_ref_existence(
                    resource_id,
                    attr_value,
                    argument_names,
                    bindings,
                    &mut all_errors,
                );
                continue;
            };

            if let Value::Deferred(DeferredValue::ResourceRef { path: ref_path }) = attr_value {
                let Some(source) = check_resource_ref_existence(
                    resource_id,
                    ref_path,
                    argument_names,
                    bindings,
                    &mut all_errors,
                ) else {
                    continue;
                };
                let source_type = source.type_in_schema();
                let sink_type = schema.type_in_schema(&attr_schema.attr_type);
                let ref_type_name = source_type.resolved_type_name();
                let expected_type_name = sink_type.resolved_type_name();

                // Directional check: source (the referenced attribute, post
                // path narrowing) must be assignable to the sink (the
                // current resource's attribute).
                if source_type.is_assignable_to(sink_type) {
                    continue;
                }

                all_errors.push(format!(
                    "{}: cannot assign {} to '{}': expected {}, got {} (from {}.{})",
                    resource_id,
                    ref_type_name,
                    attr_name,
                    expected_type_name,
                    ref_type_name,
                    ref_path.binding(),
                    ref_path.attribute(),
                ));
            } else {
                // Nested ResourceRefs are checked for binding/attribute
                // existence only. Assignability for these refs needs the
                // nested field type context from the surrounding Map/List/Struct
                // shape and should be added where that context is threaded.
                check_nested_resource_ref_existence(
                    resource_id,
                    attr_value,
                    argument_names,
                    bindings,
                    &mut all_errors,
                );
            }
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

/// Validate that attribute parameter ResourceRef values name attributes on
/// their referenced schemas, and that plain refs with a simple custom type
/// annotation are type-compatible.
///
/// For example, `attributes { role_arn: iam_role_arn = role.role_name }` should
/// be rejected because `role_name` is `String`, not `IamRoleArn`.
pub fn validate_attribute_param_ref_types_with_bindings(
    attribute_params: &[crate::parser::AttributeParameter],
    bindings: &BindingIndex<'_>,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for param in attribute_params {
        let Some(ref value) = param.value else {
            continue;
        };

        visit_refs_with_sink(
            value,
            param.type_expr.as_ref().map(RefSink::TypeExpr),
            &mut |path, sink| {
                check_attribute_param_ref(
                    &param.name,
                    sink.and_then(RefSink::as_type_expr),
                    path,
                    bindings,
                    &mut errors,
                );
            },
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn check_attribute_param_ref(
    param_name: &str,
    expected_type: Option<&TypeExpr>,
    path: &crate::resource::AccessPath,
    bindings: &BindingIndex<'_>,
    errors: &mut Vec<String>,
) {
    match bindings.ref_type(path) {
        RefType::Typed(source) => {
            let Some(sink) = expected_type.and_then(lift_type_expr) else {
                return;
            };
            let source_type = source.type_in_schema();
            let sink_type = crate::schema::TypeInSchema::schemaless(&sink);
            if !source_type.is_assignable_to(sink_type) {
                errors.push(format!(
                    "attribute '{}': type mismatch: expected {}, got {} (from {})",
                    param_name,
                    expected_type.expect("lifted expected type exists"),
                    source_type.resolved_type_name(),
                    path.to_dot_string(),
                ));
            }
        }
        RefType::UnknownAttribute(error) => {
            errors.push(format!("attribute '{}': {}", param_name, error));
        }
        RefType::Unchecked | RefType::UnknownBinding { .. } => {}
    }
}

/// Validate export parameter values that are ResourceRef against their declared
/// TypeExpr by looking up the referenced attribute's schema type.
///
/// This catches mismatches like `exports { x: list(bool) = [vpc.vpc_id] }` where
/// `vpc_id` is a string attribute but the export declares `bool`.
pub fn validate_export_param_ref_types_with_bindings(
    export_params: &[crate::parser::InferredExportParam],
    bindings: &BindingIndex<'_>,
    reported_inference_error_indices: &HashSet<usize>,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for (index, param) in export_params.iter().enumerate() {
        let Some(ref value) = param.value else {
            continue;
        };
        let unknown_type = matches!(&param.type_expr, crate::parser::TypeExpr::Unknown);
        // Inference and this walk share one dedup rule: if inference has
        // already reported this exact export-param index, skip its Unknown
        // existence walk entirely. Names are display text, not identity, and
        // can collide across sibling files.
        if unknown_type && reported_inference_error_indices.contains(&index) {
            continue;
        }

        let sink = (!unknown_type).then_some(RefSink::TypeExpr(&param.type_expr));
        visit_refs_with_sink(value, sink, &mut |path, sink| {
            check_export_ref(
                &param.name,
                sink.and_then(RefSink::as_type_expr),
                path,
                bindings,
                &mut errors,
            );
        });
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn check_export_ref(
    param_name: &str,
    expected_type: Option<&TypeExpr>,
    path: &crate::resource::AccessPath,
    bindings: &BindingIndex<'_>,
    errors: &mut Vec<String>,
) {
    match bindings.ref_type(path) {
        RefType::Typed(source) => {
            let Some(type_expr) = expected_type else {
                return;
            };
            let Some(sink) = lift_type_expr(type_expr) else {
                return;
            };
            let source_type = source.type_in_schema();
            let sink_type = crate::schema::TypeInSchema::schemaless(&sink);
            if !source_type.is_assignable_to(sink_type) {
                errors.push(format!(
                    "export '{}': type mismatch for '{}': expected {}, got {}",
                    param_name,
                    path.to_dot_string(),
                    type_expr,
                    source_type.resolved_type_name(),
                ));
            }
        }
        RefType::UnknownAttribute(error) => {
            errors.push(format!("export '{}': {}", param_name, error));
        }
        RefType::Unchecked | RefType::UnknownBinding { .. } => {}
    }
}

/// Check if an AttributeType is string-compatible (can accept a string value).
///
/// `defs` is threaded so any `Ref` receiver is peeled before shape
/// dispatch. A `Ref` target that resolves to a non-string shape
/// (typically a `Struct`) returns false, same as before — but a
/// `Ref` pointing at a string-compatible alias now answers correctly
/// instead of silently rejecting.
pub fn is_string_compatible_type(
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> bool {
    is_string_compatible_type_on_path(attr_type, defs, &[])
}

fn all_resolved_union_members_on_path<'a>(
    members: &crate::schema::UnionMembers<'a>,
    visited_refs: &mut Vec<&'a str>,
    mut predicate: impl FnMut(crate::schema::ResolvedUnionMember<'a>, &[&'a str]) -> bool,
) -> bool {
    let mut saw_member = false;
    let found_incompatible = members.any_on_path(visited_refs, |member, member_path| {
        saw_member = true;
        !predicate(member, member_path)
    });
    saw_member && !found_incompatible
}

fn is_string_compatible_type_on_path<'a>(
    attr_type: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
    visited_refs: &[&'a str],
) -> bool {
    let mut current_path = visited_refs.to_vec();
    let resolved = attr_type.resolve_refs_with_defs_on_path(defs, &mut current_path);
    let resolved_attr = resolved.as_attr();
    match resolved_attr
        .shape_ref_free()
        .expect("resolve_refs_with_defs_on_path must peel every top-level Ref")
    {
        Shape::String { .. } | Shape::Enum { .. } => true,
        Shape::Union => {
            let members = crate::schema::union_members_with_defs(resolved_attr, defs)
                .expect("Shape::Union must expose union members internally");
            all_resolved_union_members_on_path(
                &members,
                &mut current_path,
                |member, member_path| {
                    is_string_compatible_type_on_path(member.as_attr(), defs, member_path)
                },
            )
        }
        Shape::Int { .. }
        | Shape::Float { .. }
        | Shape::Bool
        | Shape::Duration
        | Shape::List { .. }
        | Shape::Map { .. }
        | Shape::Struct { .. } => false,
    }
}

/// Check that a root configuration does not contain `arguments` blocks.
///
/// `arguments` is a module-input declaration: it belongs on the module side
/// of a module boundary and is paired with `use` on the caller side. In a
/// root configuration there is no caller to pass values, so the block has
/// no meaning — its `default` would silently become a de-facto root
/// variable, which is not a documented feature (issue #2198).
///
/// A directory loaded via the CLI may be either a root config or a module
/// the user is validating in isolation. We only flag the misplaced block
/// when a `backend` or `provider` block is also present, since both are
/// root-only constructs and unambiguously identify a root configuration.
pub fn validate_no_arguments_in_root<E>(parsed: &crate::parser::File<E>) -> Result<(), String> {
    let is_root = parsed.backend.is_some() || !parsed.providers.is_empty();
    if !parsed.arguments.is_empty() && is_root {
        return Err(
            "arguments blocks are only valid inside module definitions, not in root configurations.".to_string(),
        );
    }
    Ok(())
}

/// Reject module-level type declarations whose type position names an
/// unknown bare custom type (carina#3239).
///
/// Walks every typed parameter `parsed` carries — `arguments`,
/// `attributes`, `exports` (when typed) — and applies the same
/// predicate the parser's `customs_loaded` gate uses.
///
/// The parser already rejects unknown `TypeExpr::Simple` names when
/// it is handed a `ProviderContext` with `customs_loaded = true`. That
/// gate fires for every parse path *after* the provider-registration
/// phase has populated the context — imported modules re-parsed by
/// `module_resolver::resolve_modules_with_config` and every LSP
/// diagnostic pass.
///
/// The root-config parse is the one exception: `load_configuration_with_config`
/// runs with the bootstrap context (`customs_loaded = false`) because
/// schemas have not been collected yet, so a standalone-module
/// validate (`carina validate ./my_module/`) would let an unknown
/// custom-type name in `arguments { foo: TotallyMadeUpType }` slip
/// through. This post-parse walk re-applies the same predicate against
/// the now-enriched context, closing the gap without re-parsing.
///
/// `attributes` and `exports` are covered for the same reason as
/// `arguments`: all three are module-boundary type declarations, all
/// three are reached through the same root-config parse, and an
/// unknown bare custom type in any of them surfaces the identical
/// silent-accept bug.
///
/// The check is restricted to bare PascalCase names that parsed as
/// `TypeExpr::Simple`. Dotted type expressions (`aws.iam.Role.Arn`)
/// are resolved separately by the dotted-type validation pass.
pub fn validate_argument_custom_types<E: crate::parser::ExportParamLike>(
    parsed: &crate::parser::File<E>,
    config: &ProviderContext,
) -> Vec<String> {
    let mut errors = Vec::new();
    for arg in &parsed.arguments {
        collect_unknown_simple_types_in(&arg.type_expr, config, "argument", &arg.name, &mut errors);
    }
    for ap in &parsed.attribute_params {
        if let Some(ty) = &ap.type_expr {
            collect_unknown_simple_types_in(ty, config, "attribute", &ap.name, &mut errors);
        }
    }
    for ep in &parsed.export_params {
        if let Some(ty) = ep.type_expr_opt() {
            collect_unknown_simple_types_in(ty, config, "export", ep.name(), &mut errors);
        }
    }
    errors
}

fn identity_from_dotted_path(path: &ResourceTypePath) -> TypeIdentity {
    TypeIdentity::from_dotted(&path.to_string())
}

fn resolve_dotted_unresolved(
    path: &ResourceTypePath,
    config: &ProviderContext,
) -> Result<TypeExpr, String> {
    if config.has_resource_type(&path.provider, &path.resource_type) {
        return Ok(TypeExpr::Ref(path.clone()));
    }

    let identity = identity_from_dotted_path(path);
    // Resolution confirms the written annotation names a registered type.
    // `same_type` allows wider-axis assignment, which would accept names
    // that were never registered.
    if config
        .validators
        .keys()
        .any(|registered| registered == &identity)
    {
        let schema_path = identity.segments.join(".");
        return Ok(TypeExpr::SchemaType {
            provider: path.provider.clone(),
            path: schema_path,
            type_name: identity.kind,
        });
    }

    Err(crate::parser::unknown_custom_type_message(
        &path.to_string(),
        config,
    ))
}

/// Resolve parser-produced dotted type paths against an enriched provider
/// context, recursing through every nested type-expression container.
pub fn resolve_type_expr(ty: &TypeExpr, config: &ProviderContext) -> Result<TypeExpr, String> {
    match ty {
        TypeExpr::DottedUnresolved(path) => resolve_dotted_unresolved(path, config),
        TypeExpr::List(inner) => {
            resolve_type_expr(inner, config).map(|t| TypeExpr::List(Box::new(t)))
        }
        TypeExpr::Map(inner) => {
            resolve_type_expr(inner, config).map(|t| TypeExpr::Map(Box::new(t)))
        }
        TypeExpr::Union(members) => members
            .iter()
            .map(|m| resolve_type_expr(m, config))
            .collect::<Result<Vec<_>, _>>()
            .map(TypeExpr::Union),
        TypeExpr::Struct { fields } => fields
            .iter()
            .map(|(name, field_ty)| {
                resolve_type_expr(field_ty, config).map(|resolved| (name.clone(), resolved))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|fields| TypeExpr::Struct { fields }),
        TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Simple(_)
        | TypeExpr::Ref(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::StringLiteral(_)
        | TypeExpr::Unknown => Ok(ty.clone()),
    }
}

/// Resolve every module-boundary type declaration in place. Diagnostics
/// include the declaration kind/name so callers can report the same surface
/// as [`validate_argument_custom_types`].
pub fn resolve_file_type_exprs<E: crate::parser::ExportParamLike>(
    parsed: &mut crate::parser::File<E>,
    config: &ProviderContext,
) -> Vec<String> {
    let mut errors = Vec::new();
    for arg in &mut parsed.arguments {
        match resolve_type_expr(&arg.type_expr, config) {
            Ok(resolved) => arg.type_expr = resolved,
            Err(e) => errors.push(format!("argument '{}': {e}", arg.name)),
        }
    }
    for ap in &mut parsed.attribute_params {
        if let Some(ty) = &ap.type_expr {
            match resolve_type_expr(ty, config) {
                Ok(resolved) => ap.type_expr = Some(resolved),
                Err(e) => errors.push(format!("attribute '{}': {e}", ap.name)),
            }
        }
    }
    for ep in &mut parsed.export_params {
        let export_name = ep.name().to_string();
        if let Some(ty) = ep.type_expr_opt_mut() {
            match resolve_type_expr(ty, config) {
                Ok(resolved) => *ty = resolved,
                Err(e) => errors.push(format!("export '{export_name}': {e}")),
            }
        }
    }
    errors
}

/// Recursively walk a [`TypeExpr`] and push one diagnostic per
/// [`TypeExpr::Simple`] whose name is not a known bare custom type
/// under `config`. Helper for [`validate_argument_custom_types`].
///
/// Each emitted message is a single line (no embedded newlines) so the
/// caller can `split('\n')` to lift findings into individual errors.
///
/// Variants that carry no nested `TypeExpr` are listed explicitly
/// rather than caught by a wildcard: a future variant that *does* nest
/// a `TypeExpr` should be a compile error here, not a silent
/// type-checking gap.
fn collect_unknown_simple_types_in(
    ty: &crate::parser::TypeExpr,
    config: &ProviderContext,
    decl_kind: &str,
    decl_name: &str,
    errors: &mut Vec<String>,
) {
    use crate::parser::TypeExpr;
    match ty {
        TypeExpr::Simple(snake) => {
            if !crate::parser::is_known_bare_custom_type(snake, config) {
                let pascal = crate::parser::snake_to_pascal(snake);
                errors.push(format!(
                    "{decl_kind} '{decl_name}': {}",
                    crate::parser::unknown_custom_type_message(&pascal, config)
                ));
            }
        }
        TypeExpr::DottedUnresolved(path) => {
            if let Err(e) = resolve_dotted_unresolved(path, config) {
                errors.push(format!("{decl_kind} '{decl_name}': {e}"));
            }
        }
        TypeExpr::List(inner) | TypeExpr::Map(inner) => {
            collect_unknown_simple_types_in(inner, config, decl_kind, decl_name, errors);
        }
        TypeExpr::Union(members) => {
            for m in members {
                collect_unknown_simple_types_in(m, config, decl_kind, decl_name, errors);
            }
        }
        TypeExpr::Struct { fields } => {
            for (_, field_ty) in fields {
                collect_unknown_simple_types_in(field_ty, config, decl_kind, decl_name, errors);
            }
        }
        // Leaves with no nested `TypeExpr` to recurse into. Listed
        // explicitly so a future variant that *does* nest one fails to
        // compile here instead of silently bypassing the walk.
        TypeExpr::String
        | TypeExpr::Bool
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Duration
        | TypeExpr::Ref(_)
        | TypeExpr::SchemaType { .. }
        | TypeExpr::StringLiteral(_)
        | TypeExpr::Unknown => {}
    }
}

/// Check that a module file does not contain provider blocks.
///
/// Provider configuration should only be defined at the root configuration level,
/// not inside modules (files with `arguments` or `attributes` blocks).
pub fn validate_no_provider_in_module<E>(parsed: &crate::parser::File<E>) -> Result<(), String> {
    let is_module = !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty();
    if is_module && !parsed.providers.is_empty() {
        return Err(
            "provider blocks are not allowed inside modules. Define providers at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Check that a module file does not contain state blocks.
///
/// State changes should only be defined at the root configuration level, not
/// inside modules (files with `arguments` or `attributes` blocks).
pub fn validate_no_state_blocks_in_module<E>(
    parsed: &crate::parser::File<E>,
) -> Result<(), String> {
    let is_module = !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty();
    if is_module && !parsed.state_blocks.is_empty() {
        return Err(
            "state blocks (moved, removed, and import) are not allowed inside modules. Define state blocks at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Check that a module file does not contain a backend block.
///
/// Backend configuration should only be defined at the root configuration
/// level, not inside modules (files with `arguments` or `attributes` blocks).
pub fn validate_no_backend_in_module<E>(parsed: &crate::parser::File<E>) -> Result<(), String> {
    let is_module = !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty();
    if is_module && parsed.backend.is_some() {
        return Err(
            "backend blocks are not allowed inside modules. Define the backend at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Check that a module file does not contain upstream state declarations.
///
/// Upstream state dependencies should only be defined at the root
/// configuration level, not inside modules (files with `arguments` or
/// `attributes` blocks).
pub fn validate_no_upstream_states_in_module<E>(
    parsed: &crate::parser::File<E>,
) -> Result<(), String> {
    let is_module = !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty();
    if is_module && !parsed.upstream_states.is_empty() {
        return Err(
            "upstream_state declarations are not allowed inside modules. Define upstream_state declarations at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Check that a module file does not contain an exports block.
///
/// State exports should only be defined at the root configuration level, not
/// inside modules (files with `arguments` or `attributes` blocks).
pub fn validate_no_exports_in_module<E>(parsed: &crate::parser::File<E>) -> Result<(), String> {
    let is_module = !parsed.arguments.is_empty() || !parsed.attribute_params.is_empty();
    if is_module && !parsed.export_params.is_empty() {
        return Err(
            "exports blocks are not allowed inside modules. Define exports at the root configuration level.".to_string(),
        );
    }
    Ok(())
}

/// Returns `true` if `value` contains any deferred sub-value that the
/// WASM provider boundary would reject (ResourceRef, BindingRef,
/// Interpolation, FunctionCall, Unknown). Used by
/// [`validate_provider_config`] to skip the plugin-side `validate_config`
/// call for attributes whose refs cannot be substituted at validate
/// time. `Secret` is transparent — its inner value is unwrapped because
/// the secret wrapper survives WASM serialization but the inner value
/// must still be checked. carina#3182.
pub(crate) fn value_contains_unresolved_ref(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::ResourceRef { .. })
        | Value::Deferred(DeferredValue::BindingRef { .. })
        | Value::Deferred(DeferredValue::Interpolation(_))
        | Value::Deferred(DeferredValue::FunctionCall { .. })
        | Value::Deferred(DeferredValue::Unknown(_)) => true,
        Value::Deferred(DeferredValue::Secret(inner)) => value_contains_unresolved_ref(inner),
        Value::Concrete(ConcreteValue::List(items)) => {
            items.iter().any(value_contains_unresolved_ref)
        }
        Value::Concrete(ConcreteValue::Map(map)) => map.values().any(value_contains_unresolved_ref),
        Value::Concrete(_) => false,
    }
}

/// Validate provider configuration attributes.
///
/// Runs host-side type-level validation using
/// [`ProviderFactory::provider_config_attribute_types`] first, then
/// delegates to [`ProviderFactory::validate_config`] for any
/// provider-specific semantic checks. Keeping format validation
/// (namespace structure, enum membership) on the host side means fixes
/// in `carina-core` take effect without rebuilding provider binaries.
///
/// Attributes containing unresolved references (e.g.
/// `assume_role = { role_arn = upstream.arn }` at validate time, before
/// plan/apply has fetched upstream state) are passed through host-side
/// type validation — `AttributeType::validate` is deferred-aware and
/// no-ops on `Value::Deferred` — but **excluded from the plugin-side
/// `validate_config` call**, because the WASM serializer rejects
/// deferred values. The same `validate_config` runs again at plan/apply
/// time once the refs have been substituted by
/// [`resolve_provider_attributes_with_remote`], so no plugin-side check
/// is permanently lost. carina#3182.
pub fn validate_provider_config<E>(
    parsed: &crate::parser::File<E>,
    factories: &[Box<dyn ProviderFactory>],
) -> Result<(), String> {
    for provider in &parsed.providers {
        let Some(factory) = factories.iter().find(|f| f.name() == provider.name) else {
            continue;
        };
        // Host-side type-level validation. Routed through
        // `Schema::validate_attr` with an empty `defs` because provider
        // configs are flat (no cyclic CFN-style Refs today); if a
        // future provider config grows a `Ref`, the empty-defs path
        // returns a clean `ValidationFailed` instead of tripping the
        // standalone validator sentinel (carina#3345).
        let attr_types = factory.provider_config_attribute_types();
        let schema_view = crate::schema::Schema::with_defs(std::collections::BTreeMap::new());
        for (attr_name, value) in &provider.attributes {
            if let Some(attr_type) = attr_types.get(attr_name) {
                schema_view
                    .validate_attr(attr_type, value)
                    .map_err(|e| format!("provider {}: {}: {}", provider.name, attr_name, e))?;
            }
        }
        // Plugin-side validation. Drop attributes containing unresolved
        // refs before crossing the WASM boundary; they will be checked
        // again at plan/apply time post-resolution.
        let serializable: IndexMap<String, Value> = provider
            .attributes
            .iter()
            .filter(|(_, value)| !value_contains_unresolved_ref(value))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        factory
            .validate_config(&serializable)
            .map_err(|e| format!("provider {}: {}", provider.name, e))?;
    }
    Ok(())
}

/// Validate module call arguments against module argument types.
///
/// `imported_modules` maps each module alias to its resolved boundary signature.
/// `config` provides custom type validators from providers.
pub fn validate_module_calls(
    module_calls: &[ModuleCall],
    imported_modules: &crate::module_resolver::ResolvedModuleSignatures,
    config: &ProviderContext,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for call in module_calls {
        if let Some(signature) = imported_modules.get(&call.module_name) {
            let module_args = &signature.arguments;
            for (arg_name, arg_value) in &call.arguments {
                if let Some(arg_param) = module_args.iter().find(|a| &a.name == arg_name)
                    && let Some(error) =
                        validate_type_expr_value(&arg_param.type_expr, arg_value, config)
                {
                    errors.push(format!(
                        "module {} argument '{}': {}",
                        call.module_name, arg_name, error
                    ));
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Validate export parameter values against their declared type annotations.
///
/// For each export with both a `type_expr` and a `value`, validates the value
/// using `validate_type_expr_value`. Accumulates all errors.
///
/// Accepts post-inference [`InferredExportParam`]s (#2360 stage 2):
/// `type_expr` is bare. Sentinel-bearing exports (`TypeExpr::Unknown`)
/// skip literal type compatibility here; reference existence is handled
/// separately by [`validate_export_param_ref_types_with_bindings`], with
/// inference-reported parameter indices suppressed there.
pub fn validate_export_params(
    export_params: &[crate::parser::InferredExportParam],
    config: &ProviderContext,
) -> Result<(), String> {
    let mut errors = Vec::new();

    for param in export_params {
        if matches!(&param.type_expr, crate::parser::TypeExpr::Unknown) {
            continue;
        }
        if let Some(value) = &param.value
            && let Some(error) = validate_type_expr_value(&param.type_expr, value, config)
        {
            errors.push(format!("export '{}': {}", param.name, error));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Check for unused `let` bindings and return the unused binding names.
///
/// A binding is unused if its name never appears as a value-carried reference:
/// attribute refs, bare refs, interpolation expressions, function arguments, or
/// secret inner values in resource attributes, module call arguments, attribute
/// parameters, export parameters, `directives.depends_on`, wait targets, or wait
/// `depends_on` entries.
///
/// Generic over the export-parameter shape so both `ParsedFile` (parser
/// phase) and `InferredFile` (post-loader phase) can drive it without
/// duplicating the binding walk.
pub fn check_unused_bindings<E: crate::parser::ExportParamLike>(
    parsed: &crate::parser::File<E>,
) -> Vec<String> {
    // Collect all defined binding names (skip discard pattern `_`).
    // Walk top-level and for-body resources so bindings declared inside a
    // `for` template are also tracked.
    let mut defined_bindings: Vec<String> = Vec::new();
    for rref in parsed.iter_all_resources() {
        if let Some(binding_name) = rref.binding() {
            if binding_name == "_" {
                continue;
            }
            defined_bindings.push(binding_name.to_string());
        }
    }

    if defined_bindings.is_empty() {
        return Vec::new();
    }

    // Collect all referenced binding names. Walk both top-level resources
    // and for-body template resources so bindings referenced only inside a
    // `for` loop are counted as used.
    let mut referenced: HashSet<String> = HashSet::new();
    for rref in parsed.iter_all_resources() {
        let attrs = rref.attributes();
        for (attr_name, value) in attrs.iter() {
            if attr_name.starts_with('_') {
                continue;
            }
            collect_dependencies(value, &mut referenced);
        }
        // `Composition` has no directives — `directives()` is `None`
        // for that arm, so the depends_on walk is simply skipped.
        for dep in rref.directives().into_iter().flat_map(|d| &d.depends_on) {
            referenced.insert(dep.clone());
        }
    }
    for deferred in &parsed.deferred_for_expressions {
        referenced.insert(deferred.iterable_binding.clone());
    }
    for call in &parsed.module_calls {
        for value in call.arguments.values() {
            collect_dependencies(value, &mut referenced);
        }
    }
    for attr_param in &parsed.attribute_params {
        if let Some(value) = &attr_param.value {
            collect_dependencies(value, &mut referenced);
        }
    }
    for export_param in &parsed.export_params {
        if let Some(value) = export_param.value() {
            collect_dependencies(value, &mut referenced);
        }
    }
    // Each `wait <target> { ... }` declaration references its target
    // and every binding in `depends_on = [...]`. The until predicate's
    // LHS is rooted at the target (enforced by parser), so the target
    // covers the LHS path too.
    for wb in &parsed.wait_bindings {
        referenced.insert(wb.target.as_str().to_string());
        for dep in &wb.depends_on {
            referenced.insert(dep.as_str().to_string());
        }
    }

    // Return unused binding names, skipping structurally-required bindings
    // (if/for/read expressions) and for-generated indexed bindings (e.g., vpcs[0])
    defined_bindings
        .into_iter()
        .filter(|binding| {
            !referenced.contains(binding)
                && !parsed.structural_bindings.contains(binding)
                && !binding.contains('[')
        })
        .collect()
}

/// Validate a value against a TypeExpr, returning an error message if invalid.
///
/// Shared validation logic used by both CLI module call validation and LSP diagnostics.
/// `config` provides custom type validators from providers (e.g., `iam_policy_arn`).
pub fn validate_type_expr_value(
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
) -> Option<String> {
    // `Value::Deferred(DeferredValue::Unknown)` resolves at upstream apply — the concrete type
    // is unknowable here. Same skip rule the schema validator and
    // `check_fn_arg_type` follow.
    if matches!(value, Value::Deferred(DeferredValue::Unknown(_))) {
        return None;
    }
    match (type_expr, value) {
        (TypeExpr::Simple(name), _) => {
            let identity = crate::schema::TypeIdentity::bare(crate::parser::snake_to_pascal(name));
            validate_custom_type(&identity, value, config).err()
        }
        (TypeExpr::List(inner), Value::Concrete(ConcreteValue::List(items))) => {
            for (i, item) in items.iter().enumerate() {
                if let Some(e) = validate_type_expr_value(inner, item, config) {
                    return Some(format!("Element {}: {}", i, e));
                }
            }
            None
        }
        (TypeExpr::Struct { fields }, Value::Concrete(ConcreteValue::Map(entries))) => {
            validate_struct_fields(fields, entries, config)
        }
        (TypeExpr::Struct { .. }, _) => Some(format!(
            "expected {}, got {}.",
            type_expr,
            crate::parser::value_type_name(value)
        )),
        (TypeExpr::Bool, Value::Concrete(ConcreteValue::String(s))) => Some(format!(
            "expected {type_expr}, got string \"{s}\". Use true or false."
        )),
        (TypeExpr::Int, Value::Concrete(ConcreteValue::String(s))) => {
            Some(format!("expected {type_expr}, got string \"{s}\"."))
        }
        (TypeExpr::Float, Value::Concrete(ConcreteValue::String(s))) => {
            Some(format!("expected {type_expr}, got string \"{s}\"."))
        }
        (TypeExpr::String, Value::Concrete(ConcreteValue::Bool(b))) => {
            Some(format!("expected {type_expr}, got bool ({b})."))
        }
        (TypeExpr::String, Value::Concrete(ConcreteValue::Int(n))) => {
            Some(format!("expected {type_expr}, got int ({n})."))
        }
        (TypeExpr::String, Value::Concrete(ConcreteValue::Float(f))) => {
            Some(format!("expected {type_expr}, got float ({f})."))
        }
        (TypeExpr::Bool, Value::Concrete(ConcreteValue::Int(n))) => {
            Some(format!("expected {type_expr}, got int ({n})."))
        }
        (TypeExpr::Bool, Value::Concrete(ConcreteValue::Float(f))) => {
            Some(format!("expected {type_expr}, got float ({f})."))
        }
        (TypeExpr::Int, Value::Concrete(ConcreteValue::Bool(b))) => {
            Some(format!("expected {type_expr}, got bool ({b})."))
        }
        (TypeExpr::Int, Value::Concrete(ConcreteValue::Float(f))) => {
            Some(format!("expected {type_expr}, got float ({f})."))
        }
        (TypeExpr::Float, Value::Concrete(ConcreteValue::Bool(b))) => {
            Some(format!("expected {type_expr}, got bool ({b})."))
        }
        // Intentional one-way widening: an Int may flow into a Float sink.
        // The reverse (Float -> Int) is rejected above. Mirrors the schema
        // validator's `(Float, Int) => Ok` rule in `schema/mod.rs`.
        (TypeExpr::Float, Value::Concrete(ConcreteValue::Int(_))) => None,
        // Schema and unresolved dotted types are string subtypes — reject non-string values.
        (
            TypeExpr::SchemaType { .. } | TypeExpr::DottedUnresolved(_),
            Value::Concrete(ConcreteValue::Bool(b)),
        ) => Some(format!("expected {}, got bool ({}).", type_expr, b)),
        (
            TypeExpr::SchemaType { .. } | TypeExpr::DottedUnresolved(_),
            Value::Concrete(ConcreteValue::Int(n)),
        ) => Some(format!("expected {}, got int ({}).", type_expr, n)),
        (
            TypeExpr::SchemaType { .. } | TypeExpr::DottedUnresolved(_),
            Value::Concrete(ConcreteValue::Float(f)),
        ) => Some(format!("expected {}, got float ({}).", type_expr, f)),
        _ => None,
    }
}

/// Check shape-level problems of a `Value::Concrete(ConcreteValue::Map)` against a struct field
/// list: extra keys and missing keys. Returns `None` when the key sets
/// match. Callers then walk each field with their own type-check pass.
pub fn struct_field_shape_errors(
    fields: &[(String, TypeExpr)],
    entries: &IndexMap<String, Value>,
) -> Option<String> {
    // Sort unknown keys so the diagnostic is stable across HashMap's
    // per-process random hash seed.
    let mut unknown: Vec<&String> = entries
        .keys()
        .filter(|k| !fields.iter().any(|(name, _)| &name == k))
        .collect();
    unknown.sort();
    if let Some(key) = unknown.first() {
        return Some(format!("expected struct, unknown field '{key}'."));
    }
    for (name, _) in fields {
        if !entries.contains_key(name) {
            return Some(format!("expected struct, missing field '{}'.", name));
        }
    }
    None
}

fn validate_struct_fields(
    fields: &[(String, TypeExpr)],
    entries: &IndexMap<String, Value>,
    config: &ProviderContext,
) -> Option<String> {
    if let Some(e) = struct_field_shape_errors(fields, entries) {
        return Some(e);
    }
    for (name, ty) in fields {
        if let Some(v) = entries.get(name)
            && let Some(e) = validate_type_expr_value(ty, v, config)
        {
            return Some(format!("field '{}': {}", name, e));
        }
    }
    None
}

/// Walk `field_path` against `start`. Return `Ok(tail_type)` on a
/// clean walk and `Err((mismatched_type, bad_segment))` for the first
/// segment that can't be resolved. Lists, maps, and scalars never host
/// `.field` access — the parent type they reach is the right anchor
/// for the diagnostic builder's "use iteration / subscript / nothing"
/// suggestion.
///
/// Walk `start` through a chain of `.field` segments (the leading
/// `PathSegment::Field` prefix of an [`AccessPath`]). Stops at the first
/// non-`Field` segment and returns the type at that position together
/// with the index of the segment that stopped descent — callers that
/// need to continue with subscripts use [`narrow_type_expr`], callers
/// that only care about the field-path leg use [`walk_type_expr_fields`].
///
/// Walks by reference so deep struct paths don't pay an O(depth) clone
/// chain — the caller clones once at the return site if it needs an
/// owned copy.
///
/// `Map` segments unwrap to the value type so dot-form key access
/// (`accounts.k → T`) is symmetric with the subscript form
/// (`accounts['k']`). #2447.
pub(crate) fn walk_type_expr_fields<'a, 'b>(
    start: &'a TypeExpr,
    field_path: &'b [String],
) -> Result<&'a TypeExpr, (&'a TypeExpr, &'b str)> {
    let mut current = start;
    for segment in field_path {
        match current {
            TypeExpr::Struct { fields } => match fields.iter().find(|(name, _)| name == segment) {
                Some((_, ty)) => current = ty,
                None => return Err((current, segment.as_str())),
            },
            TypeExpr::Map(inner) => current = inner.as_ref(),
            _ => return Err((current, segment.as_str())),
        }
    }
    Ok(current)
}

/// Narrow `start` through an `AccessPath`'s ordered segments — a free
/// mix of `.field` and `[index]` continuations at any depth
/// (carina#3025). Returns `None` when a step doesn't fit the container
/// kind; those mismatches are reported by the dedicated shape checkers
/// and a duplicate here would be noise.
///
/// Used by both upstream-export type-checking and module-call
/// attribute-export inference.
pub(crate) fn narrow_type_expr(
    start: &TypeExpr,
    segments: &[crate::resource::PathSegment],
) -> Option<TypeExpr> {
    use crate::resource::{PathSegment, Subscript};
    let mut current = start.clone();
    for seg in segments {
        current = match (current, seg) {
            (TypeExpr::Struct { fields }, PathSegment::Field { name }) => {
                let (_, ty) = fields.into_iter().find(|(n, _)| n == name)?;
                ty
            }
            (TypeExpr::Map(inner), PathSegment::Field { .. }) => *inner,
            (
                TypeExpr::List(inner),
                PathSegment::Subscript {
                    index: Subscript::Int { .. },
                },
            ) => *inner,
            (
                TypeExpr::Map(inner),
                PathSegment::Subscript {
                    index: Subscript::Str { .. },
                },
            ) => *inner,
            _ => return None,
        };
    }
    Some(current)
}

pub mod inference;

#[cfg(test)]
mod tests;
