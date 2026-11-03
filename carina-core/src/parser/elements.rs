//! Syntax-owned element provenance. Every location is minted from a grammar pair.
use pest::iterators::{Pair, Pairs};

use super::{DeclarationLocation, Located, PreprocessedFile, Rule};
use std::{path::Path, sync::Arc};

/// A key occurrence, including its path through maps, lists and repeated blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeKeyLocation {
    path: Vec<super::AttributePathSegment>,
    location: DeclarationLocation,
    block: bool,
}

impl AttributeKeyLocation {
    pub(in crate::parser) fn is_block(&self) -> bool {
        self.block
    }
    pub fn path(&self) -> &[super::AttributePathSegment] {
        &self.path
    }
    pub fn location(&self) -> &DeclarationLocation {
        &self.location
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElementKind {
    Provider,
    WaitTarget,
    UntilLhs,
    UntilRhs,
    Source,
    Iterable,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ElementLocations {
    pub(crate) keys: Vec<AttributeKeyLocation>,
    pub(crate) values: Vec<(ElementKind, DeclarationLocation)>,
}

impl ElementLocations {
    pub(in crate::parser) fn from_module_attributes(
        declarations: &[Located<DeclarationRecord>],
    ) -> Self {
        let keys = declarations
            .iter()
            .filter(|declaration| {
                declaration.kind() == crate::config_loader::DeclarationKind::ModuleAttribute
            })
            .map(|declaration| AttributeKeyLocation {
                path: vec![super::AttributePathSegment::Attribute(
                    declaration.name().to_string(),
                )],
                location: declaration.location().clone(),
                block: false,
            })
            .collect();
        Self {
            keys,
            values: Vec::new(),
        }
    }

    pub(in crate::parser) fn from_pair(
        pair: &Pair<'_, Rule>,
        file: &PreprocessedFile,
        path: &Path,
    ) -> Self {
        let mut out = Self::default();
        if pair.as_rule() == Rule::wait_expr {
            let mut inner = pair.clone().into_inner();
            let target = inner.next().expect("wait grammar has a target");
            out.values.push((
                ElementKind::WaitTarget,
                DeclarationLocation::from_pair(&target, file, path),
            ));
            for comparison in inner
                .flatten()
                .filter(|p| p.as_rule() == Rule::validate_comparison)
            {
                let mut operands = comparison.into_inner();
                let lhs = operands.next().expect("comparison grammar has a LHS");
                let _operator = operands.next();
                if let Some(rhs) = operands.next() {
                    out.values.push((
                        ElementKind::UntilLhs,
                        DeclarationLocation::from_pair(&lhs, file, path),
                    ));
                    out.values.push((
                        ElementKind::UntilRhs,
                        DeclarationLocation::from_pair(&rhs, file, path),
                    ));
                }
            }
        } else {
            collect_keys(pair.clone().into_inner(), &[], file, path, &mut out);
        }
        out
    }

    pub(crate) fn get(&self, kind: ElementKind) -> Option<&DeclarationLocation> {
        self.values
            .iter()
            .rev()
            .find(|(k, _)| *k == kind)
            .map(|(_, location)| location)
    }
}

fn collect_keys(
    pairs: Pairs<'_, Rule>,
    parent: &[super::AttributePathSegment],
    file: &PreprocessedFile,
    source: &Path,
    out: &mut ElementLocations,
) {
    use super::AttributePathSegment as Segment;
    let pairs: Vec<_> = pairs
        .map(|pair| {
            if pair.as_rule() == Rule::block_content {
                pair.into_inner().next().expect("block content item")
            } else {
                pair
            }
        })
        .collect();
    let mut block_indices = std::collections::HashMap::new();
    for pair in pairs {
        match pair.as_rule() {
            Rule::attribute | Rule::map_entry | Rule::nested_block | Rule::module_call_arg => {
                let is_block = pair.as_rule() == Rule::nested_block;
                let is_map_entry = pair.as_rule() == Rule::map_entry;
                let mut inner = pair.into_inner();
                let key_pair = inner.next().expect("attribute grammar has a key");
                let key = super::extract_key_string(key_pair.clone()).expect("grammar key");
                if is_block && parent.is_empty() && key == "directives" {
                    for attribute in inner.flatten().filter(|p| p.as_rule() == Rule::attribute) {
                        let mut parts = attribute.into_inner();
                        let name = parts.next().expect("directive key");
                        if name.as_str() == "provider" {
                            let value = parts.next().expect("directive value");
                            out.values.push((
                                ElementKind::Provider,
                                DeclarationLocation::from_pair(&value, file, source),
                            ));
                        }
                    }
                    continue;
                }
                let mut path = parent.to_vec();
                path.push(if parent.is_empty() {
                    Segment::Attribute(key.clone())
                } else if is_map_entry {
                    Segment::Key(key.clone())
                } else {
                    Segment::StructField(key.clone())
                });
                out.keys.push(AttributeKeyLocation {
                    path: path.clone(),
                    location: DeclarationLocation::from_pair(&key_pair, file, source),
                    block: is_block,
                });
                if is_block {
                    let index = block_indices.entry(key.clone()).or_insert(0);
                    path.push(Segment::ListIndex(*index));
                    *index += 1;
                    collect_keys(inner, &path, file, source, out);
                } else if let Some(value) = inner.next() {
                    if parent.is_empty() && key == "source" {
                        out.values.push((
                            ElementKind::Source,
                            DeclarationLocation::from_pair(&value, file, source),
                        ));
                    }
                    collect_value_keys(value, &path, file, source, out);
                }
            }
            Rule::block_content => collect_keys(pair.into_inner(), parent, file, source, out),
            _ => {}
        }
    }
}

fn collect_value_keys(
    pair: Pair<'_, Rule>,
    parent: &[super::AttributePathSegment],
    file: &PreprocessedFile,
    source: &Path,
    out: &mut ElementLocations,
) {
    if pair.as_rule() == Rule::map {
        collect_keys(pair.into_inner(), parent, file, source, out);
    } else if pair.as_rule() == Rule::list {
        for (index, value) in pair.into_inner().enumerate() {
            let mut path = parent.to_vec();
            path.push(super::AttributePathSegment::ListIndex(index));
            collect_value_keys(value, &path, file, source, out);
        }
    } else {
        for child in pair.into_inner() {
            collect_value_keys(child, parent, file, source, out);
        }
    }
}

/// Whether a declaration authored a type annotation. Absence is a distinct
/// syntax kind, rather than an optional diagnostic location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclarationAnnotation {
    Unannotated,
    Authored(DeclarationLocation),
}

/// Named declarations before any name-keyed merge. Repeated occurrences stay distinct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationRecord {
    kind: crate::config_loader::DeclarationKind,
    name: String,
    annotation: DeclarationAnnotation,
    value: Option<DeclarationLocation>,
}

impl DeclarationRecord {
    pub fn kind(&self) -> crate::config_loader::DeclarationKind {
        self.kind
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn annotation(&self) -> &DeclarationAnnotation {
        &self.annotation
    }
    pub fn value_location(&self) -> Option<&DeclarationLocation> {
        self.value.as_ref()
    }
}

pub(in crate::parser) fn declaration_inventory(
    pairs: Pairs<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
) -> Vec<Located<DeclarationRecord>> {
    use crate::config_loader::DeclarationKind as Kind;
    let mut out = Vec::new();
    for statement in pairs
        .flat_map(|p| p.into_inner())
        .filter(|p| p.as_rule() == Rule::statement)
    {
        for pair in statement.into_inner() {
            let kind = match pair.as_rule() {
                Rule::let_binding => Kind::Binding,
                Rule::fn_def => Kind::UserFunction,
                Rule::provider_block => Kind::ProviderKind,
                Rule::exports_block | Rule::arguments_block | Rule::attributes_block => {
                    let kind = match pair.as_rule() {
                        Rule::exports_block => Kind::Export,
                        Rule::arguments_block => Kind::Argument,
                        _ => Kind::ModuleAttribute,
                    };
                    for param in pair.into_inner() {
                        let mut inner = param.into_inner();
                        if let Some(name) = inner.next() {
                            let value = inner
                                .clone()
                                .find(|part| part.as_rule() == Rule::expression)
                                .map(|value| DeclarationLocation::from_pair(&value, file, source));
                            let annotation = match inner.next() {
                                Some(token) if token.as_rule() == Rule::type_expr => {
                                    DeclarationAnnotation::Authored(DeclarationLocation::from_pair(
                                        &token, file, source,
                                    ))
                                }
                                _ => DeclarationAnnotation::Unannotated,
                            };
                            out.push(Located::new(
                                DeclarationLocation::from_pair(&name, file, source),
                                DeclarationRecord {
                                    kind,
                                    name: name.as_str().to_string(),
                                    annotation,
                                    value,
                                },
                            ));
                        }
                    }
                    continue;
                }
                _ => continue,
            };
            let name = pair.into_inner().next().expect("named declaration grammar");
            if name.as_str() != "_" {
                out.push(Located::new(
                    DeclarationLocation::from_pair(&name, file, source),
                    DeclarationRecord {
                        kind,
                        name: name.as_str().to_string(),
                        annotation: DeclarationAnnotation::Unannotated,
                        value: None,
                    },
                ));
            }
        }
    }
    out
}

pub(in crate::parser) fn function_type_annotations(
    function: &Pair<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
) -> Vec<Located<crate::validation::TypeResolutionContext>> {
    use crate::validation::{
        TypeResolutionContext as Context, TypeResolutionDeclarationKind as Kind,
    };
    let mut type_annotations = Vec::new();
    let mut parts = function.clone().into_inner();
    let name = parts.next().expect("function name").as_str();
    for part in parts {
        if part.as_rule() == Rule::type_expr {
            type_annotations.push(Located::new(
                DeclarationLocation::from_pair(&part, file, source),
                Context::Declaration {
                    kind: Kind::FunctionReturn,
                    name: name.to_string(),
                },
            ));
        } else if part.as_rule() == Rule::fn_params {
            for parameter in part.into_inner() {
                let mut parts = parameter.into_inner();
                let parameter = parts.next().expect("parameter name").as_str();
                if let Some(annotation) = parts
                    .next()
                    .filter(|part| part.as_rule() == Rule::type_expr)
                {
                    type_annotations.push(Located::new(
                        DeclarationLocation::from_pair(&annotation, file, source),
                        Context::Declaration {
                            kind: Kind::FunctionParameter,
                            name: format!("{name}' parameter '{parameter}"),
                        },
                    ));
                }
            }
        }
    }
    type_annotations
}

/// Both anchors of one authored access occurrence. The prefix is the grammar's
/// `binding.field` token, while occurrence includes every field/index/postfix.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReferenceLocations {
    occurrence: DeclarationLocation,
    prefix: DeclarationLocation,
}
impl ReferenceLocations {
    pub fn occurrence(&self) -> &DeclarationLocation {
        &self.occurrence
    }
    pub fn prefix(&self) -> &DeclarationLocation {
        &self.prefix
    }
    pub(in crate::parser) fn from_pair(
        pair: &Pair<'_, Rule>,
        file: &PreprocessedFile,
        source: &Path,
    ) -> Self {
        let prefix = rooted_prefix(pair);
        let prefix = if pair.as_rule() == Rule::reference_prefix {
            DeclarationLocation::from_pair(pair, file, source)
        } else if let Some(prefix) = prefix {
            DeclarationLocation::from_pair(&prefix, file, source)
        } else {
            DeclarationLocation::reference_prefix_from_pair(pair, file, source)
        };
        Self {
            occurrence: DeclarationLocation::from_pair(pair, file, source),
            prefix,
        }
    }
    pub(crate) fn with_occurrence(&self, occurrence: DeclarationLocation) -> Self {
        Self {
            occurrence,
            prefix: self.prefix.clone(),
        }
    }
}

// Follow only the expression's root. A reference passed to a function or
// used as an index is a different occurrence, never the outer access prefix.
fn rooted_prefix<'i>(pair: &Pair<'i, Rule>) -> Option<Pair<'i, Rule>> {
    if pair.as_rule() == Rule::reference_prefix {
        return Some(pair.clone());
    }
    if matches!(
        pair.as_rule(),
        Rule::function_call | Rule::list | Rule::map | Rule::string | Rule::variable_ref
    ) {
        return None;
    }
    pair.clone()
        .into_inner()
        .next()
        .and_then(|child| rooted_prefix(&child))
}

fn has_field_access(pair: &Pair<'_, Rule>) -> bool {
    if rooted_prefix(pair).is_some() {
        return true;
    }
    if matches!(
        pair.as_rule(),
        Rule::variable_ref | Rule::postfix_expr | Rule::for_iterable
    ) && pair
        .clone()
        .into_inner()
        .any(|child| child.as_rule() == Rule::field_access)
    {
        return true;
    }
    if matches!(
        pair.as_rule(),
        Rule::function_call | Rule::list | Rule::map | Rule::string | Rule::variable_ref
    ) {
        return false;
    }
    pair.clone()
        .into_inner()
        .next()
        .is_some_and(|child| has_field_access(&child))
}

/// A deferred selection need not have a `binding.field` prefix yet. When it
/// becomes a resource path, retain the authored field prefix or the prefix
/// already owned by the selected path. Neither variant invents a range.
#[derive(Debug, Clone)]
pub(crate) enum AccessLocations {
    Reference(ReferenceLocations),
    Selection(DeclarationLocation),
}
impl AccessLocations {
    pub(in crate::parser) fn from_pair(
        pair: &Pair<'_, Rule>,
        file: &PreprocessedFile,
        source: &Path,
    ) -> Self {
        if has_field_access(pair) {
            Self::Reference(ReferenceLocations::from_pair(pair, file, source))
        } else {
            Self::Selection(DeclarationLocation::from_pair(pair, file, source))
        }
    }
    pub(crate) fn for_path(&self, path: &ReferenceProvenance) -> Option<ReferenceLocations> {
        match self {
            Self::Reference(locations) => Some(locations.clone()),
            Self::Selection(occurrence) => path
                .locations()
                .map(|locations| locations.with_occurrence(occurrence.clone())),
        }
    }
}
#[derive(Debug, Clone, Default)]
pub(crate) struct AccessProvenance(Option<Arc<AccessLocations>>);
impl AccessProvenance {
    pub(crate) fn from_locations(locations: AccessLocations) -> Self {
        Self(Some(Arc::new(locations)))
    }
    pub(crate) fn locations(&self) -> Option<&AccessLocations> {
        self.0.as_deref()
    }
}

/// Runtime metadata is ignored by semantic equality, hashing and serde.
/// Analysis validates completeness before exposing references to producers.
#[derive(Debug, Clone, Default)]
pub struct ReferenceProvenance(Option<Arc<ReferenceLocations>>);
impl ReferenceProvenance {
    pub(crate) fn shared_locations(&self) -> Option<&Arc<ReferenceLocations>> {
        self.0.as_ref()
    }
    pub(crate) fn from_locations(locations: ReferenceLocations) -> Self {
        Self(Some(Arc::new(locations)))
    }
    pub(crate) fn locations(&self) -> Option<&ReferenceLocations> {
        self.0.as_deref()
    }
}
impl PartialEq for ReferenceProvenance {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for ReferenceProvenance {}
impl std::hash::Hash for ReferenceProvenance {
    fn hash<H: std::hash::Hasher>(&self, _: &mut H) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderDeclaration {
    pub(crate) name: String,
    pub(crate) binding: Option<String>,
    pub(crate) attributes: Vec<Located<String>>,
}

fn provider_declaration(
    pair: Pair<'_, Rule>,
    binding: Option<String>,
    file: &PreprocessedFile,
    source: &Path,
) -> Located<ProviderDeclaration> {
    let mut parts = pair.into_inner();
    let name = parts.next().expect("provider name");
    let mut attributes = Vec::new();
    for attribute in parts.filter(|pair| pair.as_rule() == Rule::attribute) {
        let mut parts = attribute.into_inner();
        let key = parts
            .next()
            .expect("provider attribute key")
            .as_str()
            .to_string();
        let value = parts.next().expect("provider attribute value");
        attributes.push(Located::new(
            DeclarationLocation::from_pair(&value, file, source),
            key,
        ));
    }
    Located::new(
        DeclarationLocation::from_pair(&name, file, source),
        ProviderDeclaration {
            name: name.as_str().to_string(),
            binding,
            attributes,
        },
    )
}

/// Parser-owned inventories survive directory merging and binding phase changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationBlock {
    Arguments,
    Provider,
    State,
    Backend,
    UpstreamState,
    Exports,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyntaxInventory<
    B: crate::resource::BindingAddress = crate::resource::RelativeBindingName,
> {
    declarations: Vec<Located<DeclarationRecord>>,
    // Keep authored names for declaration diagnostics, and scope membership
    // in the binding phase so module expansion cannot leak private names.
    binding_names: Vec<B>,
    type_annotations: Vec<Located<crate::validation::TypeResolutionContext>>,
    attribute_keys: Vec<AttributeKeyLocation>,
    providers: Vec<Located<ProviderDeclaration>>,
    upstream_sources: Vec<Located<B>>,
    configuration_blocks: Vec<Located<ConfigurationBlock>>,
    backend_keys: Vec<Located<String>>,
    anonymous_headers: Vec<DeclarationLocation>,
    loops: Vec<DeclarationLocation>,
    state_targets: Vec<DeclarationLocation>,
}
impl<B: crate::resource::BindingAddress> Default for SyntaxInventory<B> {
    fn default() -> Self {
        Self {
            declarations: Vec::new(),
            binding_names: Vec::new(),
            type_annotations: Vec::new(),
            attribute_keys: Vec::new(),
            providers: Vec::new(),
            upstream_sources: Vec::new(),
            configuration_blocks: Vec::new(),
            backend_keys: Vec::new(),
            anonymous_headers: Vec::new(),
            loops: Vec::new(),
            state_targets: Vec::new(),
        }
    }
}
impl<B: crate::resource::BindingAddress> SyntaxInventory<B> {
    pub fn configuration_blocks(&self) -> &[Located<ConfigurationBlock>] {
        &self.configuration_blocks
    }
    pub(crate) fn backend_keys(&self) -> &[Located<String>] {
        &self.backend_keys
    }
    pub(crate) fn anonymous_headers(&self) -> &[DeclarationLocation] {
        &self.anonymous_headers
    }
    pub(crate) fn loops(&self) -> &[DeclarationLocation] {
        &self.loops
    }
    pub(crate) fn state_targets(&self) -> &[DeclarationLocation] {
        &self.state_targets
    }
    pub(crate) fn providers(&self) -> &[Located<ProviderDeclaration>] {
        &self.providers
    }
    pub fn attribute_keys(&self) -> &[AttributeKeyLocation] {
        &self.attribute_keys
    }
    pub fn type_annotations(&self) -> &[Located<crate::validation::TypeResolutionContext>] {
        &self.type_annotations
    }
    pub fn declarations(&self) -> &[Located<DeclarationRecord>] {
        &self.declarations
    }
    pub(crate) fn binding_names(&self) -> &[B] {
        &self.binding_names
    }
    pub fn upstream_sources(&self) -> &[Located<B>] {
        &self.upstream_sources
    }
    pub(crate) fn extend(&mut self, other: Self) {
        self.declarations.extend(other.declarations);
        self.binding_names.extend(other.binding_names);
        self.type_annotations.extend(other.type_annotations);
        self.attribute_keys.extend(other.attribute_keys);
        self.providers.extend(other.providers);
        self.upstream_sources.extend(other.upstream_sources);
        self.configuration_blocks.extend(other.configuration_blocks);
        self.backend_keys.extend(other.backend_keys);
        self.anonymous_headers.extend(other.anonymous_headers);
        self.loops.extend(other.loops);
        self.state_targets.extend(other.state_targets);
    }
    pub(crate) fn map_bindings<C: crate::resource::BindingAddress>(
        self,
        map: &mut impl FnMut(B) -> C,
    ) -> SyntaxInventory<C> {
        SyntaxInventory {
            declarations: self.declarations,
            binding_names: self.binding_names.into_iter().map(&mut *map).collect(),
            type_annotations: self.type_annotations,
            attribute_keys: self.attribute_keys,
            providers: self.providers,
            configuration_blocks: self.configuration_blocks,
            backend_keys: self.backend_keys,
            anonymous_headers: self.anonymous_headers,
            loops: self.loops,
            state_targets: self.state_targets,
            upstream_sources: self
                .upstream_sources
                .into_iter()
                .map(|source| {
                    let (location, binding) = source.into_parts();
                    Located::new(location, map(binding))
                })
                .collect(),
        }
    }
}
impl SyntaxInventory<crate::resource::AbsoluteBindingAddress> {
    pub(crate) fn map_absolute_bindings(
        &mut self,
        map: &mut impl FnMut(
            &crate::resource::AbsoluteBindingAddress,
        ) -> crate::resource::AbsoluteBindingAddress,
    ) {
        *self = std::mem::take(self).map_bindings(&mut |binding| map(&binding));
    }
}
impl SyntaxInventory {
    pub(in crate::parser) fn from_pairs(
        pairs: Pairs<'_, Rule>,
        file: &PreprocessedFile,
        source: &Path,
    ) -> Self {
        let declarations = declaration_inventory(pairs.clone(), file, source);
        use crate::validation::{
            TypeResolutionContext as Context, TypeResolutionDeclarationKind as Kind,
        };
        let mut type_annotations = Vec::new();
        for declaration in &declarations {
            if let DeclarationAnnotation::Authored(location) = declaration.annotation() {
                let kind = match declaration.kind() {
                    crate::config_loader::DeclarationKind::Argument => Kind::Argument,
                    crate::config_loader::DeclarationKind::ModuleAttribute => Kind::Attribute,
                    crate::config_loader::DeclarationKind::Export => Kind::Export,
                    _ => continue,
                };
                type_annotations.push(Located::new(
                    location.clone(),
                    Context::Declaration {
                        kind,
                        name: declaration.name().to_string(),
                    },
                ));
            }
        }
        for function in pairs
            .clone()
            .flatten()
            .filter(|pair| pair.as_rule() == Rule::fn_def)
        {
            type_annotations.extend(function_type_annotations(&function, file, source));
        }
        let mut out = Self {
            binding_names: declarations
                .iter()
                .filter(|declaration| {
                    matches!(
                        declaration.kind(),
                        crate::config_loader::DeclarationKind::Binding
                            | crate::config_loader::DeclarationKind::Argument
                            | crate::config_loader::DeclarationKind::UserFunction
                    )
                })
                .map(|declaration| crate::resource::RelativeBindingName::new(declaration.name()))
                .collect(),
            declarations,
            type_annotations,
            ..Self::default()
        };
        for statement in pairs
            .clone()
            .flat_map(|pair| pair.into_inner())
            .filter(|pair| pair.as_rule() == Rule::statement)
        {
            let pair = statement.into_inner().next().expect("statement body");
            let kind = match pair.as_rule() {
                Rule::arguments_block => Some(ConfigurationBlock::Arguments),
                Rule::provider_block => Some(ConfigurationBlock::Provider),
                Rule::moved_block | Rule::removed_block | Rule::import_state_block => {
                    Some(ConfigurationBlock::State)
                }
                Rule::backend_block => {
                    for attribute in pair
                        .clone()
                        .into_inner()
                        .filter(|part| part.as_rule() == Rule::attribute)
                    {
                        let key = attribute
                            .into_inner()
                            .next()
                            .expect("backend attribute key");
                        out.backend_keys.push(Located::new(
                            DeclarationLocation::from_pair(&key, file, source),
                            key.as_str().to_string(),
                        ));
                    }
                    Some(ConfigurationBlock::Backend)
                }
                Rule::exports_block => Some(ConfigurationBlock::Exports),
                Rule::let_binding
                    if pair.clone().into_inner().next().expect("let name").as_str() != "_"
                        && pair
                            .clone()
                            .into_inner()
                            .flatten()
                            .any(|part| part.as_rule() == Rule::upstream_state_expr) =>
                {
                    Some(ConfigurationBlock::UpstreamState)
                }
                Rule::anonymous_resource => {
                    let name = pair
                        .clone()
                        .into_inner()
                        .next()
                        .expect("anonymous resource type");
                    out.anonymous_headers
                        .push(DeclarationLocation::from_pair(&name, file, source));
                    None
                }
                _ => None,
            };
            if let Some(kind) = kind {
                out.configuration_blocks.push(Located::new(
                    DeclarationLocation::block_header_from_pair(&pair, file, source),
                    kind,
                ));
            }
        }
        for pair in pairs.clone().flatten() {
            if matches!(pair.as_rule(), Rule::for_expr | Rule::if_expr) {
                out.loops
                    .push(DeclarationLocation::from_pair(&pair, file, source));
            }
            if matches!(pair.as_rule(), Rule::moved_to_attr | Rule::import_to_attr) {
                let address = pair.into_inner().next().expect("state destination address");
                out.state_targets
                    .push(DeclarationLocation::from_pair(&address, file, source));
            }
        }
        let mut attributes = ElementLocations::default();
        for parameter in pairs
            .clone()
            .flatten()
            .filter(|pair| pair.as_rule() == Rule::attributes_param)
        {
            let mut parts = parameter.into_inner();
            let name = parts.next().expect("module attribute name").as_str();
            if let Some(value) = parts.find(|pair| pair.as_rule() == Rule::expression) {
                collect_value_keys(
                    value,
                    &[super::AttributePathSegment::Attribute(name.to_string())],
                    file,
                    source,
                    &mut attributes,
                );
            }
        }
        out.attribute_keys = attributes.keys;
        for pair in pairs
            .clone()
            .flatten()
            .filter(|pair| pair.as_rule() == Rule::provider_block)
        {
            out.providers
                .push(provider_declaration(pair, None, file, source));
        }
        for binding in pairs
            .clone()
            .flatten()
            .filter(|pair| pair.as_rule() == Rule::let_binding)
        {
            let mut parts = binding.into_inner();
            let name = parts.next().expect("let binding name").as_str().to_string();
            for provider in parts
                .flatten()
                .filter(|pair| pair.as_rule() == Rule::provider_expr)
            {
                out.providers.push(provider_declaration(
                    provider,
                    Some(name.clone()),
                    file,
                    source,
                ));
            }
        }
        for pair in pairs.flatten() {
            if pair.as_rule() == Rule::let_binding {
                let mut inner = pair.into_inner();
                let binding = inner.next().expect("let name").as_str().to_string();
                if let Some(value) = inner.next() {
                    for upstream in value
                        .into_inner()
                        .flatten()
                        .filter(|p| p.as_rule() == Rule::upstream_state_expr)
                    {
                        let elements = ElementLocations::from_pair(&upstream, file, source);
                        for (kind, location) in elements.values {
                            if kind == ElementKind::Source {
                                out.upstream_sources
                                    .push(Located::new(location, binding.clone().into()));
                            }
                        }
                    }
                }
            }
        }
        out
    }
}
