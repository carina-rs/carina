//! Authored lifecycle settings exist only before the value-expansion boundary.

use super::*;
use std::convert::Infallible;

// The directive names and typed assignment API have one definition. Every
// transformation below goes through Directives::try_map, which exhaustively
// moves the resource fields across value and binding phases.
macro_rules! lifecycle_directives {
    ($($variant:ident => $field:ident),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum LifecycleDirective { $($variant),+ }

        impl LifecycleDirective {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => stringify!($field)),+ }
            }

            pub(crate) fn from_name(name: &str) -> Option<Self> {
                match name { $(stringify!($field) => Some(Self::$variant),)+ _ => None }
            }
        }

        impl<B: BindingAddress> Directives<B, ParserPlaceholder> {
            pub(crate) fn set(&mut self, directive: LifecycleDirective, expression: DirectiveExpr<B>) {
                match directive { $(LifecycleDirective::$variant => self.$field = Some(expression)),+ }
            }
        }
    };
}

lifecycle_directives! {
    ForceDelete => force_delete,
    CreateBeforeDestroy => create_before_destroy,
    PreventDestroy => prevent_destroy,
}

mod resolved_phase {
    pub trait Sealed {}
    impl Sealed for super::NoPlaceholder {}
    impl Sealed for super::LoopSlot {}
}

#[doc(hidden)]
pub trait SerializableDirectivePhase<B: BindingAddress>:
    PlaceholderKind + resolved_phase::Sealed
{
}
impl<B: BindingAddress> SerializableDirectivePhase<B> for NoPlaceholder {}
impl<B: BindingAddress> SerializableDirectivePhase<B> for LoopSlot {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DirectiveArgumentSources(#[serde(with = "directive_sources")] Vec<DiagnosticSourceSpan>);

mod directive_sources {
    use super::*;

    #[derive(Serialize, Deserialize)]
    struct Source {
        file: PathBuf,
        range: [NonZeroUsize; 4],
    }

    pub fn serialize<S: serde::Serializer>(
        sources: &[DiagnosticSourceSpan],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        sources
            .iter()
            .filter_map(|source| source.authored_location())
            .map(|(file, range)| Source {
                file: file.to_path_buf(),
                range: [
                    range.start_line,
                    range.start_column,
                    range.end_line,
                    range.end_column,
                ],
            })
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<DiagnosticSourceSpan>, D::Error> {
        Ok(Vec::<Source>::deserialize(deserializer)?
            .into_iter()
            .map(|source| {
                let [line, column, end_line, end_column] = source.range;
                DiagnosticSourceSpan::authored(
                    source.file,
                    line.get(),
                    column.get(),
                    end_line.get(),
                    end_column.get(),
                )
            })
            .collect())
    }
}

/// Semantic provenance travels with an argument through substitution and
/// expressions and loops. Every contributing default remains attached.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArgumentValueOrigin {
    #[default]
    Explicit,
    Defaulted(ArgumentDefaultOrigins),
}

/// A non-empty, deduplicated collection of contributing defaults. The private
/// representation and deserializer prevent Defaulted from losing its cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ArgumentDefaultOrigins(Vec<ArgumentDefaultOrigin>);

impl<'de> Deserialize<'de> for ArgumentDefaultOrigins {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let defaults = Vec::<ArgumentDefaultOrigin>::deserialize(deserializer)?;
        if defaults.is_empty()
            || defaults
                .iter()
                .any(|default| default.omitted_at.0.is_empty())
        {
            return Err(serde::de::Error::custom(
                "default provenance requires an argument and its omitting call",
            ));
        }
        Ok(Self(defaults))
    }
}

impl ArgumentDefaultOrigin {
    fn diagnostic_key(&self) -> impl Ord + use<> {
        (
            self.argument.clone(),
            self.module.clone(),
            self.omitted_at
                .0
                .iter()
                .filter_map(|source| source.authored_location())
                .map(|(file, range)| {
                    (
                        file.to_path_buf(),
                        range.start_line(),
                        range.start_column(),
                        range.end_line(),
                        range.end_column(),
                    )
                })
                .collect::<Vec<_>>(),
        )
    }
}

/// Policy-bearing substitutions always carry their semantic origin. Ordinary
/// attribute sinks share the evaluator without computing unused provenance.
#[derive(Debug, Clone)]
pub(crate) struct Substituted<T> {
    value: T,
    origin: ArgumentValueOrigin,
}

impl<T> Substituted<T> {
    pub(crate) fn new(value: T, origin: ArgumentValueOrigin) -> Self {
        Self { value, origin }
    }
    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> Substituted<U> {
        Substituted {
            value: map(self.value),
            origin: self.origin,
        }
    }
    pub(crate) fn combine_origin(mut self, origin: ArgumentValueOrigin) -> Self {
        self.origin = self.origin.combine(origin);
        self
    }
    pub(crate) fn into_parts(self) -> (T, ArgumentValueOrigin) {
        (self.value, self.origin)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgumentDefaultOrigin {
    argument: String,
    module: String,
    omitted_at: DirectiveArgumentSources,
    value_shape: ArgumentDefaultShape,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum ArgumentDefaultShape {
    Bool,
    List,
    Map,
    Other,
}

impl ArgumentValueOrigin {
    pub(crate) fn defaulted(
        argument: String,
        module: String,
        call: DiagnosticSourceSpan,
        declared_type: &crate::parser::TypeExpr,
    ) -> Self {
        let value_shape = match declared_type {
            crate::parser::TypeExpr::Bool => ArgumentDefaultShape::Bool,
            crate::parser::TypeExpr::List(_) => ArgumentDefaultShape::List,
            crate::parser::TypeExpr::Map(_) => ArgumentDefaultShape::Map,
            _ => ArgumentDefaultShape::Other,
        };
        Self::Defaulted(ArgumentDefaultOrigins(vec![ArgumentDefaultOrigin {
            argument,
            module,
            omitted_at: DirectiveArgumentSources(vec![call]),
            value_shape,
        }]))
    }

    pub(crate) fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Explicit, origin) | (origin, Self::Explicit) => origin,
            (Self::Defaulted(mut left), Self::Defaulted(right)) => {
                left.0.extend(right.0);
                left.0
                    .sort_by_cached_key(ArgumentDefaultOrigin::diagnostic_key);
                left.0
                    .dedup_by(|a, b| a.diagnostic_key() == b.diagnostic_key());
                Self::Defaulted(left)
            }
        }
    }
}

impl PartialEq for DirectiveArgumentSources {
    fn eq(&self, _other: &Self) -> bool {
        // Call-site provenance is diagnostic metadata, never value identity.
        true
    }
}
impl Eq for DirectiveArgumentSources {}

impl DirectiveArgumentSources {
    fn make_relative(&mut self, root: &Path) {
        for source in &mut self.0 {
            if let Some((file, range)) = source.authored_location()
                && file.is_absolute()
            {
                let file = crate::utils::normalize_source_path(file);
                let root = crate::utils::normalize_source_path(root);
                let file_parts = file.components().collect::<Vec<_>>();
                let root_parts = root.components().collect::<Vec<_>>();
                let shared = file_parts
                    .iter()
                    .zip(&root_parts)
                    .take_while(|(a, b)| a == b)
                    .count();
                let mut relative = PathBuf::new();
                for _ in shared..root_parts.len() {
                    relative.push("..");
                }
                for component in &file_parts[shared..] {
                    relative.push(component.as_os_str());
                }
                *source = DiagnosticSourceSpan::authored(
                    relative,
                    range.start_line(),
                    range.start_column(),
                    range.end_line(),
                    range.end_column(),
                );
            }
        }
    }
}

/// An authored directive expression and the location of its value.
///
/// Only resolution can produce the `Directives` accepted by runtime consumers.
/// Authored directives cannot be serialized, even without an explicit setting:
///
/// ```compile_fail,E0277
/// use carina_core::resource::AuthoredDirectives;
/// let authored: AuthoredDirectives = AuthoredDirectives::default();
/// let _ = serde_json::to_string(&authored);
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::resource::{AuthoredResource, SavedModuleSource};
/// fn authored_resource_is_not_serializable(resource: &AuthoredResource<SavedModuleSource>) {
///     let _ = serde_json::to_string(resource);
/// }
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::resource::AuthoredDirectives;
/// let _: AuthoredDirectives = serde_json::from_str("{}").unwrap();
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::parser::DeferredForExpression;
/// use carina_core::resource::{ParserPlaceholder, RelativeBindingName, SavedModuleSource};
/// fn authored_template_is_not_serializable(
///     template: &DeferredForExpression<SavedModuleSource, ParserPlaceholder, ParserPlaceholder, RelativeBindingName>,
/// ) {
///     let _ = serde_json::to_string(template);
/// }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct DirectiveExpr<B: BindingAddress = RelativeBindingName> {
    pub(crate) value: Value<ParserPlaceholder, B>,
    pub(crate) source: DiagnosticSourceSpan,
    argument_sources: DirectiveArgumentSources,
    origin: ArgumentValueOrigin,
}

impl<B: BindingAddress> DirectiveExpr<B> {
    pub(crate) fn new(value: Value<ParserPlaceholder, B>, source: DiagnosticSourceSpan) -> Self {
        Self {
            value,
            source,
            argument_sources: DirectiveArgumentSources::default(),
            origin: ArgumentValueOrigin::Explicit,
        }
    }

    pub fn value(&self) -> &Value<ParserPlaceholder, B> {
        &self.value
    }
    pub fn source(&self) -> &DiagnosticSourceSpan {
        &self.source
    }

    pub(crate) fn with_argument_sources(
        mut self,
        sources: impl IntoIterator<Item = DiagnosticSourceSpan>,
    ) -> Self {
        self.argument_sources.0.extend(sources);
        self
    }

    fn combine_loop_origin(&mut self, origin: &ArgumentValueOrigin) {
        if self.value.references_loop_slot() {
            self.origin = std::mem::take(&mut self.origin).combine(origin.clone());
        }
    }

    pub(crate) fn try_substitute<C: BindingAddress, E>(
        self,
        map: impl FnOnce(
            Value<ParserPlaceholder, B>,
        ) -> Result<Substituted<Value<ParserPlaceholder, C>>, E>,
    ) -> Result<DirectiveExpr<C>, E> {
        let (value, origin) = map(self.value)?.into_parts();
        Ok(DirectiveExpr {
            value,
            source: self.source,
            argument_sources: self.argument_sources,
            origin: self.origin.combine(origin),
        })
    }

    pub(crate) fn try_map_value<C: BindingAddress, E>(
        self,
        map: impl FnOnce(Value<ParserPlaceholder, B>) -> Result<Value<ParserPlaceholder, C>, E>,
    ) -> Result<DirectiveExpr<C>, E> {
        Ok(DirectiveExpr {
            value: map(self.value)?,
            source: self.source,
            argument_sources: self.argument_sources,
            origin: self.origin,
        })
    }

    fn resolve_bool(
        self,
        directive: LifecycleDirective,
        resource: &ResourceId,
        schema: ProviderDirectiveSchema<'_>,
    ) -> Result<bool, DirectiveResolutionError> {
        resolve_bool(
            self.value,
            self.source,
            self.argument_sources,
            self.origin,
            directive,
            resource,
            matches!(schema, ProviderDirectiveSchema::Available(schema) if schema.prevents_destroy_by_default()),
        )
    }

    fn into_loop_template(
        self,
        directive: LifecycleDirective,
        resource: &ResourceId,
        schema: ProviderDirectiveSchema<'_>,
        element_type: Option<&crate::parser::TypeExpr>,
    ) -> Result<LoopDirective<bool, B>, DirectiveResolutionError> {
        let has_loop_slot = self.value.references_loop_slot();
        let slot_type = match &self.value {
            Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::Loop(
                LoopSlot::Value,
            ))) => element_type.cloned(),
            Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::Loop(LoopSlot::Key))) => {
                Some(crate::parser::TypeExpr::String)
            }
            Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::Loop(
                LoopSlot::Index,
            ))) => Some(crate::parser::TypeExpr::Int),
            _ => None,
        };
        if let Some(kind) = slot_type
            && kind != crate::parser::TypeExpr::Bool
        {
            let mut error = self.resolve_bool(directive, resource, schema).unwrap_err();
            error.0.found = format!("got {kind} from the loop element");
            return Err(error);
        }
        let is_local = loop_expression_is_local(&self.value);
        if has_loop_slot
            && is_local
            && let Ok(value) = self.value.clone().into_loop_template()
        {
            return Ok(LoopDirective::PerElement(Box::new(LoopDirectiveExpr {
                value,
                source_range: DirectiveSourceRange::from_span(&self.source),
                source: self.source,
                argument_sources: self.argument_sources,
                origin: self.origin,
                schema_protects: matches!(schema, ProviderDirectiveSchema::Available(schema) if schema.prevents_destroy_by_default()),
            })));
        }
        self.resolve_bool(directive, resource, schema)
            .map(LoopDirective::Resolved)
            .map_err(|mut error| {
                if !is_local {
                    error.0.found = "loop directive must depend only on the current element (coalesce, if, and resource references are not allowed)".into();
                }
                error
            })
    }
}

fn resolve_bool<P: PlaceholderKind, B: BindingAddress>(
    value: Value<P, B>,
    source: DiagnosticSourceSpan,
    argument_sources: DirectiveArgumentSources,
    origin: ArgumentValueOrigin,
    directive: LifecycleDirective,
    resource: &ResourceId,
    schema_protects: bool,
) -> Result<bool, DirectiveResolutionError> {
    match value {
        Value::Concrete(ConcreteValue::Bool(value)) => {
            if !value && directive == LifecycleDirective::PreventDestroy && schema_protects {
                match origin {
                    ArgumentValueOrigin::Explicit => {}
                    ArgumentValueOrigin::Defaulted(default) => {
                        return Err(DirectiveResolutionError(Box::new(
                            DirectiveResolutionErrorDetails {
                                directive,
                                resource: resource.to_string(),
                                found: String::new(),
                                source,
                                argument_sources,
                                pending: PendingDirectiveValue::Other,
                                defaulted_opt_out: Some(default),
                            },
                        )));
                    }
                }
            }
            Ok(value)
        }
        value => {
            let pending = match &value {
                Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
                    if let Some(name) = placeholder.module_argument_name() {
                        PendingDirectiveValue::Argument(name.to_owned())
                    } else if let Some(slot) = placeholder.loop_slot() {
                        PendingDirectiveValue::Loop(slot)
                    } else {
                        PendingDirectiveValue::Other
                    }
                }
                Value::Deferred(_) => {
                    let mut module_arguments = false;
                    let mut loop_slots = false;
                    value.visit_placeholders(&mut |placeholder| {
                        module_arguments |= placeholder.module_argument_name().is_some();
                        loop_slots |= placeholder.loop_slot().is_some();
                    });
                    PendingDirectiveValue::Computed {
                        module_arguments,
                        loop_slots,
                    }
                }
                Value::Concrete(_) => PendingDirectiveValue::Other,
            };
            Err(DirectiveResolutionError(Box::new(
                DirectiveResolutionErrorDetails {
                    directive,
                    resource: resource.to_string(),
                    found: directive_value_kind(&value),
                    source,
                    argument_sources,
                    pending,
                    defaulted_opt_out: None,
                },
            )))
        }
    }
}

fn loop_expression_is_local<P: PlaceholderKind, B: BindingAddress>(value: &Value<P, B>) -> bool {
    match value {
        Value::Concrete(ConcreteValue::List(values)) => values.iter().all(loop_expression_is_local),
        Value::Concrete(ConcreteValue::Map(values)) => {
            values.values().all(loop_expression_is_local)
        }
        Value::Concrete(_) => true,
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
            placeholder.loop_slot().is_some()
        }
        Value::Deferred(DeferredValue::Access(access)) => loop_expression_is_local(access.base()),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            parts.iter().all(|part| match part {
                InterpolationPart::Literal(_) => true,
                InterpolationPart::Expr(value) => loop_expression_is_local(value),
            })
        }
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => args.iter().all(|argument| {
            argument.handle().is_none() && loop_expression_is_local(argument.walker_value())
        }),
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            loop_expression_is_local(expression.lhs()) && loop_expression_is_local(expression.rhs())
        }
        Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::Coalesce { .. }
            | DeferredValue::ModuleIf(_)
            | DeferredValue::Unavailable { .. }
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. },
        ) => false,
    }
}

fn directive_value_kind<P: PlaceholderKind, B: BindingAddress>(value: &Value<P, B>) -> String {
    match value {
        Value::Concrete(value) => format!(
            "got {}",
            match value {
                ConcreteValue::String(_) => "String",
                ConcreteValue::Int(_) => "Int",
                ConcreteValue::Float(_) => "Float",
                ConcreteValue::Bool(_) => "Bool",
                ConcreteValue::Duration(_) => "Duration",
                ConcreteValue::EnumIdentifier(_) | ConcreteValue::CanonicalEnum(_) => "Enum",
                ConcreteValue::List(_) | ConcreteValue::StringList(_) => "List",
                ConcreteValue::Map(_) => "Map",
            }
        ),
        Value::Deferred(DeferredValue::ResourceRef { .. }) => {
            "got a resource reference (unknown until apply)".into()
        }
        Value::Deferred(DeferredValue::Unknown(_)) => "unknown until apply".into(),
        Value::Deferred(DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. }) => {
            "got a secret value".into()
        }
        Value::Deferred(_) => "got a deferred expression".into(),
    }
}

/// A loop-template setting cannot be read as a runtime boolean. Its expression
/// must first be substituted and checked for the individual child resource.
/// Resolved variants retain the historical scalar wire format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    untagged,
    bound(
        serialize = "T: Serialize, B: Serialize",
        deserialize = "T: Deserialize<'de>, B: Deserialize<'de>"
    )
)]
pub enum LoopDirective<T, B: BindingAddress> {
    Resolved(T),
    PerElement(Box<LoopDirectiveExpr<B>>),
}

impl<T: Default, B: BindingAddress> Default for LoopDirective<T, B> {
    fn default() -> Self {
        Self::Resolved(T::default())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DirectiveSourceRange(Option<[NonZeroUsize; 4]>);

impl PartialEq for DirectiveSourceRange {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl DirectiveSourceRange {
    fn from_span(source: &DiagnosticSourceSpan) -> Self {
        Self(source.authored_location().map(|(_, range)| {
            [
                range.start_line,
                range.start_column,
                range.end_line,
                range.end_column,
            ]
        }))
    }

    fn in_file(&self, file: &Path) -> DiagnosticSourceSpan {
        match self.0 {
            Some([line, column, end_line, end_column]) => DiagnosticSourceSpan::authored(
                file.to_path_buf(),
                line.get(),
                column.get(),
                end_line.get(),
                end_column.get(),
            ),
            None => DiagnosticSourceSpan::deserialized(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "B: Serialize", deserialize = "B: Deserialize<'de>"))]
pub struct LoopDirectiveExpr<B: BindingAddress> {
    value: Value<LoopSlot, B>,
    // Saved templates retain the exact range; their owning loop supplies the
    // portable filename on materialization instead of saving an absolute path.
    source_range: DirectiveSourceRange,
    #[serde(skip, default = "DiagnosticSourceSpan::deserialized")]
    source: DiagnosticSourceSpan,
    argument_sources: DirectiveArgumentSources,
    origin: ArgumentValueOrigin,
    schema_protects: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingDirectiveValue {
    Argument(String),
    Loop(LoopSlot),
    Computed {
        module_arguments: bool,
        loop_slots: bool,
    },
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(transparent)]
pub struct DirectiveResolutionError(Box<DirectiveResolutionErrorDetails>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectiveResolutionErrorDetails {
    directive: LifecycleDirective,
    resource: String,
    found: String,
    source: DiagnosticSourceSpan,
    argument_sources: DirectiveArgumentSources,
    pending: PendingDirectiveValue,
    defaulted_opt_out: Option<ArgumentDefaultOrigins>,
}

impl DirectiveResolutionError {
    pub fn directive(&self) -> &'static str {
        self.0.directive.as_str()
    }
    pub fn resource(&self) -> &str {
        &self.0.resource
    }
    pub fn source_span(&self) -> &DiagnosticSourceSpan {
        self.diagnostic_sources().next().unwrap_or(&self.0.source)
    }

    /// The editor owns source anchoring; messages contain no location prefix.
    pub fn message(&self) -> String {
        if let Some(defaults) = &self.0.defaulted_opt_out {
            let describe = |default: &ArgumentDefaultOrigin| {
                format!(
                    "the default of argument `{}` of module `{}`",
                    default.argument, default.module
                )
            };
            let examples = defaults
                .0
                .iter()
                .map(|default| {
                    let value = match default.value_shape {
                        ArgumentDefaultShape::Bool => "false",
                        ArgumentDefaultShape::List => "[false]",
                        ArgumentDefaultShape::Map => "{ key = false }",
                        ArgumentDefaultShape::Other => "<value>",
                    };
                    format!("`{} = {value}`", default.argument)
                })
                .collect::<Vec<_>>()
                .join(", ");
            return format!(
                "directives.prevent_destroy for resource `{}`: false came from {}; the caller must pass {examples} explicitly to opt out of schema-default protection",
                self.0.resource,
                defaults
                    .0
                    .iter()
                    .map(describe)
                    .collect::<Vec<_>>()
                    .join(" and ")
            );
        }
        format!(
            "directives.{} must resolve to a Bool for resource `{}`; {}",
            self.0.directive.as_str(),
            self.0.resource,
            self.0.found,
        )
    }

    pub fn source_in_file(&self, file: &Path) -> Option<DiagnosticSourceRange> {
        self.diagnostic_sources()
            .filter_map(DiagnosticSourceSpan::authored_location)
            .find_map(|(source, range)| (source == file).then_some(range))
    }

    fn diagnostic_sources(&self) -> impl Iterator<Item = &DiagnosticSourceSpan> {
        self.0
            .defaulted_opt_out
            .iter()
            .flat_map(|defaults| defaults.0.iter().flat_map(|default| &default.omitted_at.0))
            .chain(self.0.defaulted_opt_out.is_none().then_some(&self.0.source))
            .chain(
                self.0
                    .argument_sources
                    .0
                    .iter()
                    .filter(|_| self.0.defaulted_opt_out.is_none()),
            )
    }

    pub(crate) fn pending_bool_module_argument(
        &mut self,
        arguments: &HashMap<&str, &crate::parser::TypeExpr>,
        element_type: Option<&crate::parser::TypeExpr>,
        argument_iterable: bool,
    ) -> bool {
        match &self.0.pending {
            PendingDirectiveValue::Argument(name) => match arguments.get(name.as_str()) {
                Some(crate::parser::TypeExpr::Bool) => true,
                Some(kind) => {
                    self.0.found = format!("got {kind} from module argument `{name}`");
                    false
                }
                None => false,
            },
            PendingDirectiveValue::Loop(LoopSlot::Value) => {
                if argument_iterable && let Some(kind) = element_type {
                    if *kind == crate::parser::TypeExpr::Bool {
                        return true;
                    }
                    self.0.found = format!("got {kind} from the loop element");
                }
                false
            }
            PendingDirectiveValue::Computed {
                module_arguments,
                loop_slots,
            } => {
                if *module_arguments || (*loop_slots && argument_iterable) {
                    self.0.found = "cannot be checked until the module is instantiated".into();
                }
                false
            }
            PendingDirectiveValue::Loop(LoopSlot::Index | LoopSlot::Key)
            | PendingDirectiveValue::Other => false,
        }
    }

    /// Identity for deduplicating a located error, including its authored site.
    pub fn diagnostic_key(&self) -> impl Eq + Hash + use<> {
        let location = |span: &DiagnosticSourceSpan| {
            span.authored_location()
                .map(|(file, range)| (file.to_path_buf(), range))
        };
        (
            self.0.directive,
            location(&self.0.source),
            self.diagnostic_sources()
                .filter_map(location)
                .collect::<Vec<_>>(),
        )
    }
}

impl Display for DirectiveResolutionError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some((file, range)) = self.source_span().authored_location() {
            write!(
                f,
                "{}:{}:{}: ",
                file.display(),
                range.start_line(),
                range.start_column()
            )?;
        }
        f.write_str(&self.message())?;
        let mut seen = HashSet::new();
        if let Some(location) = self.source_span().authored_location() {
            seen.insert(location);
        }
        for (file, range) in self
            .diagnostic_sources()
            .filter_map(DiagnosticSourceSpan::authored_location)
        {
            if seen.insert((file, range)) {
                write!(
                    f,
                    " ({} {}:{}:{})",
                    if self.0.defaulted_opt_out.is_some() {
                        "also omitted at"
                    } else {
                        "called at"
                    },
                    file.display(),
                    range.start_line(),
                    range.start_column()
                )?;
            }
        }
        Ok(())
    }
}
impl std::error::Error for DirectiveResolutionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectiveResolutionErrors(Vec<DirectiveResolutionError>);

impl DirectiveResolutionErrors {
    pub(crate) fn new(errors: Vec<DirectiveResolutionError>) -> Self {
        Self(errors)
    }
    pub fn errors(&self) -> &[DirectiveResolutionError] {
        &self.0
    }
    pub fn into_errors(self) -> Vec<DirectiveResolutionError> {
        self.0
    }
}
impl Display for DirectiveResolutionErrors {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index != 0 {
                f.write_str("\n")?;
            }
            error.fmt(f)?;
        }
        Ok(())
    }
}
impl std::error::Error for DirectiveResolutionErrors {}

/// A file must resolve both parser slots and directive expressions.
#[derive(Debug, thiserror::Error)]
pub enum ExpansionError {
    #[error(transparent)]
    Placeholders(#[from] LeftoverPlaceholders),
    #[error(transparent)]
    Directives(#[from] DirectiveResolutionErrors),
}
impl From<LeftoverPlaceholder> for ExpansionError {
    fn from(error: LeftoverPlaceholder) -> Self {
        Self::Placeholders(error.into())
    }
}
impl From<DirectiveResolutionError> for ExpansionError {
    fn from(error: DirectiveResolutionError) -> Self {
        Self::Directives(DirectiveResolutionErrors(vec![error]))
    }
}
impl ExpansionError {
    pub fn located(&self) -> impl Display + '_ {
        struct Located<'a>(&'a ExpansionError);
        impl Display for Located<'_> {
            fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                match self.0 {
                    ExpansionError::Placeholders(errors) => errors.located().fmt(f),
                    ExpansionError::Directives(errors) => errors.fmt(f),
                }
            }
        }
        Located(self)
    }
}

impl<B: BindingAddress, P: PlaceholderKind> Directives<B, P> {
    /// The only exhaustive field mapping across directive phases.
    fn try_map<C: BindingAddress, Q: PlaceholderKind, E, Context>(
        self,
        context: &mut Context,
        mut boolean: impl FnMut(
            &mut Context,
            LifecycleDirective,
            P::DirectiveBool<B>,
        ) -> Result<Q::DirectiveBool<C>, E>,
        prevention: impl FnOnce(
            &mut Context,
            LifecycleDirective,
            P::DirectivePreventDestroy<B>,
        ) -> Result<Q::DirectivePreventDestroy<C>, E>,
    ) -> Result<Directives<C, Q>, E> {
        let Self {
            force_delete,
            create_before_destroy,
            prevent_destroy,
            depends_on_diagnostic_sources,
            ignore_changes,
            provider_instance,
        } = self;
        Ok(Directives {
            force_delete: boolean(context, LifecycleDirective::ForceDelete, force_delete)?,
            create_before_destroy: boolean(
                context,
                LifecycleDirective::CreateBeforeDestroy,
                create_before_destroy,
            )?,
            prevent_destroy: prevention(
                context,
                LifecycleDirective::PreventDestroy,
                prevent_destroy,
            )?,
            depends_on_diagnostic_sources,
            ignore_changes,
            provider_instance,
        })
    }
}

fn infallible<T>(result: Result<T, Infallible>) -> T {
    match result {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

impl<B: BindingAddress> Directives<B, ParserPlaceholder> {
    pub(crate) fn combine_loop_origin(&mut self, origin: &ArgumentValueOrigin) {
        self.map_expressions(|expression| expression.combine_loop_origin(origin));
    }

    pub(crate) fn try_map_expressions<C: BindingAddress, E, F>(
        self,
        mut map: F,
    ) -> Result<Directives<C, ParserPlaceholder>, E>
    where
        F: FnMut(LifecycleDirective, DirectiveExpr<B>) -> Result<DirectiveExpr<C>, E>,
    {
        let map_option = |map: &mut F, name, expression: Option<DirectiveExpr<B>>| {
            expression
                .map(|expression| map(name, expression))
                .transpose()
        };
        self.try_map(&mut map, map_option, map_option)
    }

    pub(crate) fn try_update_expressions<E>(
        &mut self,
        mut map: impl FnMut(LifecycleDirective, &mut DirectiveExpr<B>) -> Result<(), E>,
    ) -> Result<(), E> {
        *self = std::mem::take(self).try_map_expressions(|name, mut expression| {
            map(name, &mut expression)?;
            Ok(expression)
        })?;
        Ok(())
    }

    pub(crate) fn map_expressions(&mut self, mut map: impl FnMut(&mut DirectiveExpr<B>)) {
        infallible(self.try_update_expressions(|_, expression| {
            map(expression);
            Ok(())
        }));
    }

    /// Analysis uses precisely the same resolver as the publication boundary.
    pub(crate) fn resolution_errors(
        &self,
        resource: &ResourceId,
        schema: ProviderDirectiveSchema<'_>,
        loop_template: bool,
        element_type: Option<&crate::parser::TypeExpr>,
        iterable: Option<&Value<ParserPlaceholder, B>>,
    ) -> Vec<DirectiveResolutionError> {
        let result = if loop_template {
            self.clone()
                .into_loop_template(resource, schema, element_type)
                .and_then(|template| template.validate_known_elements(resource, iterable))
        } else {
            self.clone().resolve(resource, schema).map(|_| ())
        };
        result
            .err()
            .map(DirectiveResolutionErrors::into_errors)
            .unwrap_or_default()
    }

    pub fn resolve(
        self,
        resource: &ResourceId,
        schema: ProviderDirectiveSchema<'_>,
    ) -> Result<Directives<B>, DirectiveResolutionErrors> {
        fn setting<B: BindingAddress>(
            errors: &mut Vec<DirectiveResolutionError>,
            name: LifecycleDirective,
            expression: Option<DirectiveExpr<B>>,
            resource: &ResourceId,
            schema: ProviderDirectiveSchema<'_>,
        ) -> Option<bool> {
            expression.and_then(|expression| {
                match expression.resolve_bool(name, resource, schema) {
                    Ok(value) => Some(value),
                    Err(error) => {
                        errors.push(error);
                        None
                    }
                }
            })
        }
        let mut errors = Vec::new();
        let resolved = infallible(self.try_map(
            &mut errors,
            |errors, name, expression| {
                Ok(setting(errors, name, expression, resource, schema).unwrap_or(false))
            },
            |errors, name, expression| {
                Ok(setting(errors, name, expression, resource, schema)
                    .map(PreventDestroy::Explicit)
                    .unwrap_or(PreventDestroy::Unspecified))
            },
        ));
        if errors.is_empty() {
            Ok(resolved)
        } else {
            Err(DirectiveResolutionErrors(errors))
        }
    }

    pub(crate) fn into_loop_template(
        self,
        resource: &ResourceId,
        schema: ProviderDirectiveSchema<'_>,
        element_type: Option<&crate::parser::TypeExpr>,
    ) -> Result<Directives<B, LoopSlot>, DirectiveResolutionErrors> {
        let mut errors = Vec::new();
        let setting =
            |errors: &mut Vec<_>, name, expression: Option<DirectiveExpr<B>>| match expression
                .map(|expression| {
                    expression.into_loop_template(name, resource, schema, element_type)
                })
                .transpose()
            {
                Ok(value) => value,
                Err(error) => {
                    errors.push(error);
                    None
                }
            };
        let resolved = infallible(self.try_map(
            &mut errors,
            |errors, name, expression| Ok(setting(errors, name, expression).unwrap_or_default()),
            |errors, name, expression| {
                Ok(match setting(errors, name, expression) {
                    Some(LoopDirective::Resolved(value)) => {
                        LoopDirective::Resolved(PreventDestroy::Explicit(value))
                    }
                    Some(LoopDirective::PerElement(expression)) => {
                        LoopDirective::PerElement(expression)
                    }
                    None => LoopDirective::Resolved(PreventDestroy::Unspecified),
                })
            },
        ));
        if errors.is_empty() {
            Ok(resolved)
        } else {
            Err(DirectiveResolutionErrors(errors))
        }
    }
}

impl<T, B: BindingAddress> LoopDirective<T, B> {
    fn make_sources_relative(&mut self, root: &Path) {
        if let Self::PerElement(expression) = self {
            expression.argument_sources.make_relative(root);
            if let ArgumentValueOrigin::Defaulted(defaults) = &mut expression.origin {
                for default in &mut defaults.0 {
                    default.omitted_at.make_relative(root);
                }
            }
        }
    }
    fn map_bindings<C: BindingAddress>(
        self,
        resolve: &mut impl FnMut(B) -> C,
    ) -> LoopDirective<T, C> {
        match self {
            Self::Resolved(value) => LoopDirective::Resolved(value),
            Self::PerElement(expression) => {
                LoopDirective::PerElement(Box::new(LoopDirectiveExpr {
                    value: expression.value.map_bindings(resolve),
                    source: expression.source,
                    source_range: expression.source_range,
                    argument_sources: expression.argument_sources,
                    origin: expression.origin,
                    schema_protects: expression.schema_protects,
                }))
            }
        }
    }

    fn resolve<E: Display>(
        self,
        name: LifecycleDirective,
        resource: &ResourceId,
        source_file: &Path,
        substitute: &mut impl FnMut(Value<LoopSlot, B>) -> Result<Value<NoPlaceholder, B>, E>,
        explicit: impl FnOnce(bool) -> T,
    ) -> Result<T, DirectiveResolutionError> {
        match self {
            Self::Resolved(value) => Ok(value),
            Self::PerElement(expression) => {
                let source = if expression.source.authored_location().is_some() {
                    expression.source
                } else {
                    expression.source_range.in_file(source_file)
                };
                let value = substitute(expression.value).map_err(|error| {
                    DirectiveResolutionError(Box::new(DirectiveResolutionErrorDetails {
                        directive: name,
                        resource: resource.to_string(),
                        found: format!("could not resolve the loop value: {error}"),
                        source: source.clone(),
                        argument_sources: expression.argument_sources.clone(),
                        pending: PendingDirectiveValue::Other,
                        defaulted_opt_out: None,
                    }))
                })?;
                resolve_bool(
                    value,
                    source,
                    expression.argument_sources,
                    expression.origin,
                    name,
                    resource,
                    expression.schema_protects,
                )
                .map(explicit)
            }
        }
    }
}

impl<B: BindingAddress> Directives<B, LoopSlot> {
    fn validate_known_elements(
        &self,
        resource: &ResourceId,
        iterable: Option<&Value<ParserPlaceholder, B>>,
    ) -> Result<(), DirectiveResolutionErrors> {
        let Some(iterable) = iterable.filter(|value| value.is_fully_resolved()) else {
            return Ok(());
        };
        let Ok(iterable) = iterable.clone().into_expanded() else {
            return Ok(());
        };
        let mut errors = Vec::new();
        let mut element = |index, key, value| {
            if let Err(error) =
                self.clone()
                    .substitute(resource, Path::new("<loop>"), |expression| {
                        crate::parser::resolve_loop_directive_value(
                            expression,
                            index,
                            key,
                            value,
                            AccessMissPolicy::Error,
                        )
                    })
            {
                errors.extend(error.into_errors());
            }
        };
        match &iterable {
            Value::Concrete(ConcreteValue::List(values)) => {
                for (index, value) in values.iter().enumerate() {
                    element(
                        Some(
                            index
                                .try_into()
                                .expect("for-loop index fits in the signed address domain"),
                        ),
                        None,
                        value,
                    );
                }
            }
            Value::Concrete(ConcreteValue::Map(values)) => {
                for (key, value) in values {
                    element(None, Some(key.as_str()), value);
                }
            }
            _ => {}
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(DirectiveResolutionErrors(errors))
        }
    }

    pub(crate) fn make_sources_relative(&mut self, root: &Path) {
        *self = infallible(self.clone().try_map(
            &mut (),
            |_, _, mut value| {
                value.make_sources_relative(root);
                Ok(value)
            },
            |_, _, mut value| {
                value.make_sources_relative(root);
                Ok(value)
            },
        ));
    }
    pub(crate) fn map_expression_bindings(&mut self, map: &mut impl FnMut(B) -> B) {
        *self = infallible(self.clone().try_map(
            map,
            |map, _, value| Ok(value.map_bindings(map)),
            |map, _, value| Ok(value.map_bindings(map)),
        ));
    }

    pub(crate) fn substitute<E: Display>(
        self,
        resource: &ResourceId,
        source_file: &Path,
        mut substitute: impl FnMut(Value<LoopSlot, B>) -> Result<Value<NoPlaceholder, B>, E>,
    ) -> Result<Directives<B>, DirectiveResolutionErrors> {
        let mut errors = Vec::new();
        let resolved = infallible(self.try_map(
            &mut (&mut errors, &mut substitute),
            |(errors, substitute), name, value| {
                Ok(collect_setting(
                    value.resolve(name, resource, source_file, substitute, |value| value),
                    errors,
                ))
            },
            |(errors, substitute), name, value| {
                Ok(collect_setting(
                    value.resolve(
                        name,
                        resource,
                        source_file,
                        substitute,
                        PreventDestroy::Explicit,
                    ),
                    errors,
                ))
            },
        ));
        if errors.is_empty() {
            Ok(resolved)
        } else {
            Err(DirectiveResolutionErrors(errors))
        }
    }
}

impl<B: BindingAddress> Directives<B> {
    /// Lift already resolved settings into a loop template without changing
    /// their scalar wire representation.
    pub fn into_loop_template(self) -> Directives<B, LoopSlot> {
        infallible(self.try_map(
            &mut (),
            |_, _, value| Ok(LoopDirective::Resolved(value)),
            |_, _, value| Ok(LoopDirective::Resolved(value)),
        ))
    }
}

fn collect_setting<T: Default>(
    result: Result<T, DirectiveResolutionError>,
    errors: &mut Vec<DirectiveResolutionError>,
) -> T {
    match result {
        Ok(value) => value,
        Err(error) => {
            errors.push(error);
            T::default()
        }
    }
}

pub(crate) fn rebind_loop_directives<B: BindingAddress, C: BindingAddress>(
    directives: Directives<B, LoopSlot>,
    resolve: &mut impl FnMut(B) -> C,
) -> Directives<C, LoopSlot> {
    infallible(directives.try_map(
        resolve,
        |resolve, _, value| Ok(value.map_bindings(resolve)),
        |resolve, _, value| Ok(value.map_bindings(resolve)),
    ))
}

pub(crate) fn rebind_authored_directives<B: BindingAddress, C: BindingAddress>(
    directives: Directives<B, ParserPlaceholder>,
    resolve: &mut impl FnMut(B) -> C,
) -> Directives<C, ParserPlaceholder> {
    let expression = |resolve: &mut _, _, expression: Option<DirectiveExpr<B>>| {
        expression
            .map(|expression| expression.try_map_value(|value| Ok(value.map_bindings(resolve))))
            .transpose()
    };
    infallible(directives.try_map(resolve, expression, expression))
}

pub(crate) fn rebind_resolved_directives<B, C, P, Q>(
    directives: Directives<B, P>,
    resolve: &mut impl FnMut(B) -> C,
) -> Directives<C, Q>
where
    B: BindingAddress,
    C: BindingAddress,
    P: PlaceholderKind<DirectiveBool<B> = bool, DirectivePreventDestroy<B> = PreventDestroy>,
    Q: PlaceholderKind<DirectiveBool<C> = bool, DirectivePreventDestroy<C> = PreventDestroy>,
{
    infallible(directives.try_map(resolve, |_, _, value| Ok(value), |_, _, value| Ok(value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_origin(argument: &str, line: usize) -> ArgumentValueOrigin {
        ArgumentValueOrigin::defaulted(
            argument.to_owned(),
            "./module".into(),
            DiagnosticSourceSpan::authored("caller.crn".into(), line, 1, line, 20),
            &crate::parser::TypeExpr::Bool,
        )
    }

    #[test]
    fn argument_origins_combine_all_defaults_and_distinct_omitting_calls() {
        let first = default_origin("first", 3);
        let second = default_origin("second", 7);
        let other_call = default_origin("first", 8);
        let combined = first
            .clone()
            .combine(second.clone())
            .combine(other_call.clone());
        let reverse = other_call.combine(second).combine(first.clone());
        // Compare encoded provenance because source metadata deliberately does
        // not participate in value equality.
        let encoded = serde_json::to_value(&combined).unwrap();
        assert_eq!(encoded, serde_json::to_value(reverse).unwrap());
        assert_eq!(encoded["Defaulted"].as_array().unwrap().len(), 3);
        assert_eq!(
            encoded,
            serde_json::to_value(combined.clone().combine(first)).unwrap()
        );
        assert_eq!(
            encoded,
            serde_json::to_value(combined.clone().combine(ArgumentValueOrigin::Explicit)).unwrap()
        );
        assert_eq!(
            encoded,
            serde_json::to_value(ArgumentValueOrigin::Explicit.combine(combined)).unwrap()
        );
    }

    #[test]
    fn default_origin_wire_requires_a_non_empty_list_with_omitting_calls() {
        let origin = default_origin("protect", 3);
        let encoded = serde_json::to_value(&origin).unwrap();
        let decoded: ArgumentValueOrigin = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            serde_json::to_value(origin).unwrap()
        );
        assert!(
            serde_json::from_value::<ArgumentValueOrigin>(serde_json::json!({"Defaulted": []}))
                .is_err()
        );
        assert!(serde_json::from_value::<ArgumentValueOrigin>(serde_json::json!({"Defaulted": [{
            "argument": "protect", "module": "./module", "omitted_at": [], "value_shape": "Bool",
        }]})).is_err());
        assert!(
            serde_json::from_value::<ArgumentValueOrigin>(serde_json::json!({
                "Defaulted": encoded["Defaulted"][0],
            }))
            .is_err(),
            "v38 provenance must be a list, not an unshipped single-object shape"
        );
    }

    #[test]
    fn loop_directives_reject_dependencies_outside_the_element_at_validation() {
        for invalid in [
            Value::resource_ref("peer", "flag", Vec::new()),
            Value::Deferred(DeferredValue::BindingRef {
                binding: RelativeBindingName::new("peer"),
            }),
            Value::Deferred(DeferredValue::Unknown(
                ApplyTimeUnknown::UpstreamAllExports {
                    binding: RelativeBindingName::new("upstream"),
                },
            )),
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::Deferred(DeferredValue::Placeholder(
                    ParserPlaceholder::Loop(LoopSlot::Value),
                ))),
                default: Box::new(Value::Concrete(ConcreteValue::Bool(false))),
            }),
            Value::Deferred(DeferredValue::ModuleIf(Box::new(
                crate::parser::DeferredValueIf::new(
                    crate::parser::DeferredIfCondition::new(
                        Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::Loop(
                            LoopSlot::Value,
                        ))),
                        "loops.crn".into(),
                        3,
                    ),
                    Value::Concrete(ConcreteValue::Bool(true)),
                    Value::Concrete(ConcreteValue::Bool(false)),
                ),
            ))),
        ] {
            let mut authored: AuthoredDirectives = AuthoredDirectives::default();
            authored.force_delete = Some(DirectiveExpr::new(
                invalid,
                DiagnosticSourceSpan::authored("loops.crn".into(), 3, 5, 3, 20),
            ));
            let error = authored
                .into_loop_template(
                    &ResourceId::with_identity("mock.test.resource", "child"),
                    crate::resource::ProviderDirectiveSchema::Unavailable,
                    None,
                )
                .expect_err("non-local directive dependencies must fail before expansion");
            assert!(
                error.to_string().contains("loops.crn:3:5:")
                    && error.to_string().contains("force_delete")
            );
            assert!(
                error
                    .to_string()
                    .contains("loop directive must depend only on the current element"),
                "{error}"
            );
            assert!(
                !error.to_string().contains("unknown until apply"),
                "{error}"
            );
        }
    }

    #[test]
    fn legacy_scalar_template_directives_deserialize_as_resolved() {
        let template: Directives<AbsoluteBindingAddress, LoopSlot> =
            serde_json::from_value(serde_json::json!({
                "force_delete": true,
                "create_before_destroy": false,
                "prevent_destroy": {"kind": "explicit", "value": false},
            }))
            .unwrap();
        assert!(matches!(
            template.force_delete,
            LoopDirective::Resolved(true)
        ));
        assert!(matches!(
            template.create_before_destroy,
            LoopDirective::Resolved(false)
        ));
        assert!(matches!(
            template.prevent_destroy,
            LoopDirective::Resolved(PreventDestroy::Explicit(false))
        ));
    }

    #[test]
    fn loop_directive_binding_remapping_visits_each_binding_once() {
        let root = RootScope::new();
        let source = root.resolve(RelativeBindingName::new("source"));
        let mut authored: AuthoredDirectives<AbsoluteBindingAddress> =
            AuthoredDirectives::default();
        authored.force_delete = Some(DirectiveExpr::new(
            Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
                DeferredValue::Placeholder(ParserPlaceholder::Loop(LoopSlot::Value)),
            )])),
            DiagnosticSourceSpan::authored("module.crn".into(), 3, 1, 3, 10),
        ));
        let template = authored
            .into_loop_template(
                &ResourceId::with_identity("mock.test.resource", "target"),
                crate::resource::ProviderDirectiveSchema::Unavailable,
                None,
            )
            .unwrap();
        let mut resource = Resource::<ModuleSource, LoopSlot>::with_provider(
            "mock",
            "test.resource",
            "target",
            None,
        );
        resource.directives = template;
        resource.dependencies.depends_on = crate::parser::UnexpandedDependencies::new(vec![
            crate::parser::UnexpandedDependencyOperand::BareBinding(source),
        ]);
        let mut visited = Vec::new();
        resource.map_absolute_bindings(&mut |binding| {
            visited.push(binding.to_string());
            root.resolve(RelativeBindingName::new(format!("remapped.{binding}")))
        });
        assert_eq!(visited, ["source"]);
        assert_eq!(
            resource
                .dependencies
                .depends_on
                .iter()
                .next()
                .unwrap()
                .binding_reference()
                .as_str(),
            "remapped.source"
        );
        let LoopDirective::PerElement(expression) = &resource.directives.force_delete else {
            panic!("expected a loop expression");
        };
        let mut references = Vec::new();
        expression
            .value
            .visit_resource_refs(&mut |reference| references.push(reference.binding().to_string()));
        assert!(references.is_empty());
    }

    #[test]
    fn directive_argument_sources_do_not_change_authored_value_equality() {
        let expression: DirectiveExpr = DirectiveExpr::new(
            Value::Concrete(ConcreteValue::Bool(true)),
            DiagnosticSourceSpan::authored("module.crn".into(), 1, 1, 1, 5),
        );
        let forwarded = expression
            .clone()
            .with_argument_sources([DiagnosticSourceSpan::authored(
                "caller.crn".into(),
                3,
                10,
                3,
                14,
            )]);
        assert_eq!(expression, forwarded);
    }

    #[test]
    fn authored_directives_reject_non_bool_deferred_and_unknown_values() {
        for name in ["prevent_destroy", "create_before_destroy", "force_delete"] {
            for (value, expected) in [
                (
                    Value::Concrete(ConcreteValue::String("true".into())),
                    "got String",
                ),
                (Value::Concrete(ConcreteValue::Int(1)), "got Int"),
                (
                    Value::resource_ref(RelativeBindingName::new("resource"), "flag", vec![]),
                    "got a resource reference (unknown until apply)",
                ),
                (
                    Value::Deferred(DeferredValue::Unknown(
                        ApplyTimeUnknown::UpstreamAllExports {
                            binding: RelativeBindingName::new("upstream"),
                        },
                    )),
                    "unknown until apply",
                ),
                (
                    Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::Loop(
                        LoopSlot::Value,
                    ))),
                    "got a deferred expression",
                ),
            ] {
                let mut directives = AuthoredDirectives::default();
                directives.set(
                    LifecycleDirective::from_name(name).unwrap(),
                    DirectiveExpr::new(
                        value,
                        DiagnosticSourceSpan::authored("module/main.crn".into(), 7, 5, 7, 10),
                    ),
                );
                let error = directives
                    .resolve(
                        &ResourceId::with_identity("mock.test.resource", "target"),
                        crate::resource::ProviderDirectiveSchema::Unavailable,
                    )
                    .expect_err("invalid values must never become defaults");
                assert_eq!(error.errors().len(), 1);
                assert!(
                    std::mem::size_of_val(&error.errors()[0]) <= std::mem::size_of::<[usize; 2]>(),
                    "directive errors must remain small enough to return by value"
                );
                assert_eq!(error.errors()[0].directive(), name);
                assert!(error.to_string().contains("module/main.crn:7:5:"));
                assert!(error.to_string().ends_with(expected), "{error}");
            }
        }
    }

    #[test]
    fn resolved_directive_wire_format_remains_compatible() {
        for (value, prevent_destroy) in [
            (None, serde_json::json!({"kind": "unspecified"})),
            (
                Some(false),
                serde_json::json!({"kind": "explicit", "value": false}),
            ),
            (
                Some(true),
                serde_json::json!({"kind": "explicit", "value": true}),
            ),
        ] {
            let mut authored = AuthoredDirectives::default();
            if let Some(value) = value {
                for name in ["prevent_destroy", "create_before_destroy", "force_delete"] {
                    authored.set(
                        LifecycleDirective::from_name(name).unwrap(),
                        DirectiveExpr::new(
                            Value::Concrete(ConcreteValue::Bool(value)),
                            DiagnosticSourceSpan::authored("main.crn".into(), 1, 1, 1, 5),
                        ),
                    );
                }
            }
            let resolved = NoPlaceholder::rebind_directives(
                authored
                    .resolve(
                        &ResourceId::with_identity("mock.test.resource", "target"),
                        crate::resource::ProviderDirectiveSchema::Unavailable,
                    )
                    .unwrap(),
                &mut |binding| RootScope::new().resolve(binding),
            );
            let encoded = serde_json::to_value(&resolved).unwrap();
            assert_eq!(
                encoded,
                serde_json::json!({
                    "force_delete": value.unwrap_or(false),
                    "create_before_destroy": value.unwrap_or(false),
                    "prevent_destroy": prevent_destroy,
                })
            );
            let decoded: Directives = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded, resolved);
            let persisted = PersistedDirectives::from_resolved(
                &decoded,
                PersistedDestroyDependencies::default(),
            );
            assert_eq!(
                serde_json::to_value(&persisted).unwrap(),
                serde_json::to_value(&decoded).unwrap()
            );
        }
    }
}
