use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::parser::{SourceSpan, display_string_literal};

use super::ResourceIdentity;

/// The authored construct that produced an anonymous resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopKind {
    For,
    If,
}

impl std::fmt::Display for LoopKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::For => formatter.write_str("for"),
            Self::If => formatter.write_str("if"),
        }
    }
}

/// The collection element selected by a `for` expression.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ForLoopElement {
    Index(usize),
    Key(String),
}

impl ForLoopElement {
    pub(crate) fn address(&self, base: &str) -> String {
        match self {
            Self::Index(index) => format!("{base}[{index}]"),
            Self::Key(key) => crate::utils::map_key_address(base, key),
        }
    }
}

/// The precise branch or collection element that produced a resource.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopOriginKind {
    For { element: ForLoopElement },
    If,
}

impl LoopOriginKind {
    pub(crate) fn loop_kind(&self) -> LoopKind {
        match self {
            Self::For { .. } => LoopKind::For,
            Self::If => LoopKind::If,
        }
    }
}

/// The address emitted by Carina before anonymous loop elements were fixed.
///
/// This type is deliberately crate-private and has no string or live-identity
/// conversion. Only migration and diagnostic code may inspect its contents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct LegacyLoopAddress(String);

impl LegacyLoopAddress {
    pub(crate) fn new(address: impl Into<String>) -> Self {
        Self(address.into())
    }

    pub(crate) fn exact_state_name(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn with_parent_prefix(&self, prefix: &ResourceIdentity) -> Self {
        Self(format!("{}.{}", prefix.as_str(), self.0))
    }

    pub(crate) fn with_parent_legacy(&self, prefix: &Self) -> Self {
        Self(format!("{}.{}", prefix.exact_state_name(), self.0))
    }

    fn remap_identity_prefix(&mut self, from: &ResourceIdentity, to: &ResourceIdentity) {
        if let Some(rewritten) =
            replace_identity_prefix(self.exact_state_name(), from.as_str(), to.as_str())
        {
            self.0 = rewritten;
        }
    }
}

#[derive(Debug, Clone)]
struct LoopAuthoring {
    span: SourceSpan,
    source_text: Arc<str>,
    binding_scope: Arc<BTreeSet<String>>,
}

/// Source identity shared by every element produced from one unbound loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopSource {
    display_file: String,
    line: usize,
    column: usize,
    #[serde(default = "default_loop_kind")]
    kind: LoopKind,
    system_identity: ResourceIdentity,
    #[serde(skip, default)]
    authoring: Option<LoopAuthoring>,
}

impl PartialEq for LoopSource {
    fn eq(&self, other: &Self) -> bool {
        self.display_file == other.display_file
            && self.line == other.line
            && self.column == other.column
            && self.kind == other.kind
            && self.system_identity == other.system_identity
    }
}

impl Eq for LoopSource {}

impl Hash for LoopSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.display_file.hash(state);
        self.line.hash(state);
        self.column.hash(state);
        self.kind.hash(state);
        self.system_identity.hash(state);
    }
}

impl LoopSource {
    pub(crate) fn authored(
        source_file: &Path,
        span: SourceSpan,
        source_text: impl Into<Arc<str>>,
        kind: LoopKind,
    ) -> Self {
        let normalized = crate::utils::normalize_source_path(source_file);
        let display_file = normalized.display().to_string();
        let system_identity = loop_system_identity(
            &display_file,
            span.start_byte(),
            span.end_byte(),
            kind,
            None,
        );
        Self {
            display_file,
            line: span.start_line(),
            column: span.start_column(),
            kind,
            system_identity,
            authoring: Some(LoopAuthoring {
                span,
                source_text: source_text.into(),
                binding_scope: Arc::new(BTreeSet::new()),
            }),
        }
    }

    pub fn file(&self) -> &str {
        &self.display_file
    }

    pub fn line(&self) -> usize {
        self.line
    }

    pub fn column(&self) -> usize {
        self.column
    }

    pub(crate) fn scheduler_key(&self) -> &str {
        self.system_identity.as_str()
    }

    pub(crate) fn same_system_target(&self, other: &Self) -> bool {
        self.system_identity == other.system_identity
    }

    pub(crate) fn source_text(&self) -> Option<&str> {
        self.authoring
            .as_ref()
            .map(|authoring| authoring.source_text.as_ref())
    }

    pub(crate) fn source_span(&self) -> Option<SourceSpan> {
        self.authoring.as_ref().map(|authoring| authoring.span)
    }

    pub(crate) fn binding_scope(&self) -> Option<&BTreeSet<String>> {
        self.authoring
            .as_ref()
            .map(|authoring| authoring.binding_scope.as_ref())
    }

    pub(crate) fn set_binding_scope(&mut self, binding_scope: Arc<BTreeSet<String>>) {
        if let Some(authoring) = &mut self.authoring {
            authoring.binding_scope = binding_scope;
        }
    }

    pub(crate) fn set_display_file(&mut self, file: String, kind: LoopKind) {
        self.kind = kind;
        if let Some(span) = self.authoring.as_ref().map(|authoring| authoring.span) {
            self.system_identity =
                loop_system_identity(&file, span.start_byte(), span.end_byte(), kind, None);
        }
        self.display_file = file;
    }

    pub(crate) fn scope_to_module(&mut self, scope: &ResourceIdentity) {
        self.system_identity = loop_system_identity(
            self.system_identity.as_str(),
            self.line,
            self.column,
            self.kind,
            Some(scope.as_str()),
        );
    }
}

const fn default_loop_kind() -> LoopKind {
    LoopKind::For
}

fn loop_system_identity(
    file_or_parent_identity: &str,
    start: usize,
    end: usize,
    kind: LoopKind,
    module_scope: Option<&str>,
) -> ResourceIdentity {
    fn component(hasher: &mut Sha256, value: &[u8]) {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }

    let mut hasher = Sha256::new();
    component(&mut hasher, file_or_parent_identity.as_bytes());
    hasher.update((start as u64).to_be_bytes());
    hasher.update((end as u64).to_be_bytes());
    hasher.update([match kind {
        LoopKind::For => 0,
        LoopKind::If => 1,
    }]);
    match module_scope {
        Some(scope) => {
            hasher.update([1]);
            component(&mut hasher, scope.as_bytes());
        }
        None => hasher.update([0]),
    }
    ResourceIdentity::new(format!("_loop_{}", hex::encode(&hasher.finalize()[..8])))
}

/// Provenance for one concrete resource emitted by an unbound `for` or `if`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LoopElementOrigin {
    kind: LoopOriginKind,
    source: LoopSource,
    legacy: LegacyLoopAddress,
}

impl LoopElementOrigin {
    pub(crate) fn new(kind: LoopOriginKind, source: LoopSource, legacy: LegacyLoopAddress) -> Self {
        Self {
            kind,
            source,
            legacy,
        }
    }

    pub fn kind(&self) -> &LoopOriginKind {
        &self.kind
    }

    pub fn file(&self) -> &str {
        self.source.file()
    }

    pub fn line(&self) -> usize {
        self.source.line()
    }

    pub fn display_suffix(&self) -> String {
        let mut suffix = String::new();
        self.append_display_suffix(&mut suffix);
        suffix
    }

    pub fn append_display_suffix(&self, output: &mut String) {
        use std::fmt::Write as _;

        match &self.kind {
            LoopOriginKind::For {
                element: ForLoopElement::Index(index),
            } => write!(output, "(for [{index}] at {}:{})", self.file(), self.line()),
            LoopOriginKind::For {
                element: ForLoopElement::Key(key),
            } => write!(
                output,
                "(for [{}] at {}:{})",
                display_string_literal(key),
                self.file(),
                self.line()
            ),
            LoopOriginKind::If => write!(output, "(if at {}:{})", self.file(), self.line()),
        }
        .expect("writing to a String cannot fail");
    }

    pub(crate) fn source(&self) -> &LoopSource {
        &self.source
    }

    pub(crate) fn source_mut(&mut self) -> &mut LoopSource {
        &mut self.source
    }

    pub(crate) fn set_display_file(&mut self, file: &str) {
        self.source
            .set_display_file(file.to_string(), self.kind.loop_kind());
    }

    pub(crate) fn legacy_address(&self) -> &LegacyLoopAddress {
        &self.legacy
    }

    #[cfg(test)]
    pub(crate) fn prefix_legacy(&mut self, prefix: &ResourceIdentity) {
        self.legacy = self.legacy.with_parent_prefix(prefix);
    }

    pub(crate) fn prefix_legacy_address(&mut self, prefix: &LegacyLoopAddress) {
        self.legacy = self.legacy.with_parent_legacy(prefix);
    }

    fn remap_legacy_identity_prefix(&mut self, from: &ResourceIdentity, to: &ResourceIdentity) {
        self.legacy.remap_identity_prefix(from, to);
    }

    pub(crate) fn set_binding_scope(&mut self, binding_scope: Arc<BTreeSet<String>>) {
        self.source.set_binding_scope(binding_scope);
    }

    pub(crate) fn rewritten_address(&self, binding: &str) -> String {
        let element_suffix = match &self.kind {
            LoopOriginKind::For { element } => element.address(""),
            LoopOriginKind::If => String::new(),
        };
        let legacy = self.legacy.exact_state_name();
        let legacy_base = legacy.strip_suffix(&element_suffix).unwrap_or(legacy);
        let parent = legacy_base
            .rsplit_once('.')
            .map(|(parent, _)| format!("{parent}."))
            .unwrap_or_default();
        format!("{parent}{binding}{element_suffix}")
    }
}

/// A loop-generated module instance and the old prefix it replaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct LoopModulePrefixOrigin {
    origin: LoopElementOrigin,
    current_prefix: ResourceIdentity,
}

impl LoopModulePrefixOrigin {
    pub(crate) fn new(origin: LoopElementOrigin, current_prefix: ResourceIdentity) -> Self {
        Self {
            origin,
            current_prefix,
        }
    }

    pub(crate) fn origin(&self) -> &LoopElementOrigin {
        &self.origin
    }

    pub(crate) fn origin_mut(&mut self) -> &mut LoopElementOrigin {
        &mut self.origin
    }

    pub(crate) fn current_prefix(&self) -> &ResourceIdentity {
        &self.current_prefix
    }

    pub(crate) fn legacy_prefix(&self) -> &LegacyLoopAddress {
        self.origin.legacy_address()
    }

    pub(crate) fn remap_current_prefix(&mut self, from: &ResourceIdentity, to: &ResourceIdentity) {
        if let Some(rewritten) =
            replace_identity_prefix(self.current_prefix.as_str(), from.as_str(), to.as_str())
        {
            self.current_prefix = ResourceIdentity::new(rewritten);
        }
    }

    fn prefix_for_module(
        &mut self,
        current_parent: &ResourceIdentity,
        legacy_parent: &LegacyLoopAddress,
    ) {
        self.current_prefix = ResourceIdentity::new(format!(
            "{}.{}",
            current_parent.as_str(),
            self.current_prefix.as_str(),
        ));
        self.origin.prefix_legacy_address(legacy_parent);
        self.origin.source_mut().scope_to_module(current_parent);
    }

    fn remap_identity_prefix(&mut self, from: &ResourceIdentity, to: &ResourceIdentity) {
        self.remap_current_prefix(from, to);
        self.origin.remap_legacy_identity_prefix(from, to);
    }
}

/// Host-owned provenance for resources emitted directly or through a module.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopProvenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    direct: Option<Box<LoopElementOrigin>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    module_prefixes: Vec<LoopModulePrefixOrigin>,
}

impl LoopProvenance {
    pub(crate) fn direct(origin: LoopElementOrigin) -> Self {
        Self {
            direct: Some(Box::new(origin)),
            module_prefixes: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.direct.is_none() && self.module_prefixes.is_empty()
    }

    pub fn display_origin(&self) -> Option<&LoopElementOrigin> {
        self.direct.as_deref().or_else(|| {
            self.module_prefixes
                .last()
                .map(LoopModulePrefixOrigin::origin)
        })
    }

    pub(crate) fn direct_origin(&self) -> Option<&LoopElementOrigin> {
        self.direct.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn direct_origin_mut(&mut self) -> Option<&mut LoopElementOrigin> {
        self.direct.as_deref_mut()
    }

    #[cfg(test)]
    pub(crate) fn module_prefixes(&self) -> &[LoopModulePrefixOrigin] {
        &self.module_prefixes
    }

    pub(crate) fn push_module_prefix(&mut self, origin: LoopModulePrefixOrigin) {
        self.module_prefixes.push(origin);
    }

    pub(crate) fn prefix_for_module(
        &mut self,
        current_parent: &ResourceIdentity,
        legacy_parent: &LegacyLoopAddress,
    ) {
        if let Some(origin) = &mut self.direct {
            origin.prefix_legacy_address(legacy_parent);
            origin.source_mut().scope_to_module(current_parent);
        }
        for prefix in &mut self.module_prefixes {
            prefix.prefix_for_module(current_parent, legacy_parent);
        }
    }

    pub(crate) fn remap_identity_prefix(&mut self, from: &ResourceIdentity, to: &ResourceIdentity) {
        if let Some(origin) = &mut self.direct {
            origin.remap_legacy_identity_prefix(from, to);
        }
        for prefix in &mut self.module_prefixes {
            prefix.remap_identity_prefix(from, to);
        }
    }

    pub(crate) fn set_display_file(&mut self, file: &str) {
        if let Some(origin) = &mut self.direct {
            origin.set_display_file(file);
        }
        for prefix in &mut self.module_prefixes {
            prefix.origin_mut().set_display_file(file);
        }
    }

    pub(crate) fn set_binding_scope(&mut self, binding_scope: Arc<BTreeSet<String>>) {
        if let Some(origin) = &mut self.direct {
            origin.set_binding_scope(Arc::clone(&binding_scope));
        }
        for prefix in &mut self.module_prefixes {
            prefix
                .origin_mut()
                .set_binding_scope(Arc::clone(&binding_scope));
        }
    }

    pub(crate) fn legacy_candidates(
        &self,
        current_identity: &ResourceIdentity,
    ) -> Vec<LegacyLoopAddress> {
        let mut candidates = Vec::new();
        if let Some(origin) = &self.direct {
            candidates.push(origin.legacy_address().clone());
        }

        let mut module_prefixes: Vec<_> = self.module_prefixes.iter().collect();
        module_prefixes
            .sort_by_key(|prefix| std::cmp::Reverse(prefix.current_prefix().as_str().len()));
        for prefix in module_prefixes {
            if let Some(legacy) = replace_identity_prefix(
                current_identity.as_str(),
                prefix.current_prefix().as_str(),
                prefix.legacy_prefix().exact_state_name(),
            ) {
                candidates.push(LegacyLoopAddress::new(legacy));
            }
        }

        candidates.dedup();
        candidates
    }
}

/// Source and legacy migration template for a deferred unbound `for`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UnboundLoopTemplate {
    source: LoopSource,
    legacy_base: LegacyLoopAddress,
}

impl UnboundLoopTemplate {
    pub(crate) fn new(source: LoopSource, legacy_base: LegacyLoopAddress) -> Self {
        Self {
            source,
            legacy_base,
        }
    }

    pub(crate) fn scheduler_key(&self) -> &str {
        self.source.scheduler_key()
    }

    pub(crate) fn same_system_target(&self, other: &Self) -> bool {
        self.source.same_system_target(&other.source)
    }

    pub(crate) fn element_origin(&self, element: ForLoopElement) -> LoopElementOrigin {
        let legacy = LegacyLoopAddress::new(element.address(self.legacy_base.exact_state_name()));
        LoopElementOrigin::new(LoopOriginKind::For { element }, self.source.clone(), legacy)
    }

    pub(crate) fn if_origin(&self) -> LoopElementOrigin {
        LoopElementOrigin::new(
            LoopOriginKind::If,
            self.source.clone(),
            self.legacy_base.clone(),
        )
    }

    pub(crate) fn unbound_read_error(&self, kind: LoopKind) -> Option<crate::parser::ParseError> {
        Some(crate::parser::ParseError::UnboundLoopRead {
            kind,
            line: self.source.line(),
            span: self.source.source_span()?,
        })
    }

    pub(crate) fn matches_legacy_binding(&self, binding: &str) -> bool {
        binding == self.legacy_base.exact_state_name()
            || binding
                .strip_prefix(self.legacy_base.exact_state_name())
                .is_some_and(|suffix| suffix.starts_with(['[', '.']))
    }

    pub(crate) fn set_display_file(&mut self, file: &str) {
        self.source
            .set_display_file(file.to_string(), LoopKind::For);
    }

    pub(crate) fn set_binding_scope(&mut self, binding_scope: Arc<BTreeSet<String>>) {
        self.source.set_binding_scope(binding_scope);
    }

    #[cfg(test)]
    pub(crate) fn legacy_base(&self) -> &LegacyLoopAddress {
        &self.legacy_base
    }

    pub(crate) fn prefix_for_module(
        &mut self,
        current_parent: &ResourceIdentity,
        legacy_parent: &LegacyLoopAddress,
    ) {
        self.source.scope_to_module(current_parent);
        self.legacy_base = self.legacy_base.with_parent_legacy(legacy_parent);
    }
}

pub(crate) fn replace_identity_prefix(value: &str, from: &str, to: &str) -> Option<String> {
    let suffix = value.strip_prefix(from)?;
    if !suffix.is_empty() && !suffix.starts_with('.') {
        return None;
    }
    Some(format!("{to}{suffix}"))
}
