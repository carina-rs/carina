use std::collections::BTreeSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

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

    pub(crate) fn with_parent_prefix(&self, prefix: &ResourceIdentity) -> Self {
        Self(format!("{}.{}", prefix.as_str(), self.0))
    }
}

#[derive(Debug, Clone)]
struct LoopAuthoring {
    span: SourceSpan,
    source_text: Arc<str>,
    occupied_bindings: Arc<BTreeSet<String>>,
}

/// Source identity shared by every element produced from one unbound loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopSource {
    display_file: String,
    line: usize,
    column: usize,
    system_identity: ResourceIdentity,
    #[serde(skip, default)]
    authoring: Option<LoopAuthoring>,
}

impl PartialEq for LoopSource {
    fn eq(&self, other: &Self) -> bool {
        self.display_file == other.display_file
            && self.line == other.line
            && self.column == other.column
            && self.system_identity == other.system_identity
    }
}

impl Eq for LoopSource {}

impl Hash for LoopSource {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.display_file.hash(state);
        self.line.hash(state);
        self.column.hash(state);
        self.system_identity.hash(state);
    }
}

impl LoopSource {
    pub(crate) fn authored(
        source_file: &Path,
        span: SourceSpan,
        source_text: impl Into<Arc<str>>,
        kind: LoopKind,
        occupied_bindings: BTreeSet<String>,
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
            system_identity,
            authoring: Some(LoopAuthoring {
                span,
                source_text: source_text.into(),
                occupied_bindings: Arc::new(occupied_bindings),
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

    pub(crate) fn occupied_bindings(&self) -> Option<&BTreeSet<String>> {
        self.authoring
            .as_ref()
            .map(|authoring| authoring.occupied_bindings.as_ref())
    }

    pub(crate) fn set_display_file(&mut self, file: String, kind: LoopKind) {
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
            LoopKind::For,
            Some(scope.as_str()),
        );
    }
}

fn loop_system_identity(
    file_or_parent_identity: &str,
    start: usize,
    end: usize,
    kind: LoopKind,
    module_scope: Option<&str>,
) -> ResourceIdentity {
    let mut hasher = DefaultHasher::new();
    file_or_parent_identity.hash(&mut hasher);
    start.hash(&mut hasher);
    end.hash(&mut hasher);
    kind.hash(&mut hasher);
    module_scope.hash(&mut hasher);
    ResourceIdentity::new(format!("_loop_{:016x}", hasher.finish()))
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

    pub fn column(&self) -> usize {
        self.source.column()
    }

    pub fn display_suffix(&self) -> String {
        match &self.kind {
            LoopOriginKind::For {
                element: ForLoopElement::Index(index),
            } => format!("(for [{index}] at {}:{})", self.file(), self.line()),
            LoopOriginKind::For {
                element: ForLoopElement::Key(key),
            } => format!(
                "(for [{}] at {}:{})",
                display_string_literal(key),
                self.file(),
                self.line()
            ),
            LoopOriginKind::If => format!("(if at {}:{})", self.file(), self.line()),
        }
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

    pub(crate) fn prefix_legacy(&mut self, prefix: &ResourceIdentity) {
        self.legacy = self.legacy.with_parent_prefix(prefix);
    }

    pub(crate) fn rewritten_address(&self, binding: &str) -> String {
        let element_suffix = match &self.kind {
            LoopOriginKind::For {
                element: ForLoopElement::Index(index),
            } => format!("[{index}]"),
            LoopOriginKind::For {
                element: ForLoopElement::Key(key),
            } => crate::utils::map_key_address("", key),
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
    legacy_prefix: LegacyLoopAddress,
}

impl LoopModulePrefixOrigin {
    pub(crate) fn new(
        origin: LoopElementOrigin,
        current_prefix: ResourceIdentity,
        legacy_prefix: LegacyLoopAddress,
    ) -> Self {
        Self {
            origin,
            current_prefix,
            legacy_prefix,
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
        &self.legacy_prefix
    }

    pub(crate) fn remap_current_prefix(&mut self, from: &ResourceIdentity, to: &ResourceIdentity) {
        if let Some(rewritten) =
            replace_identity_prefix(self.current_prefix.as_str(), from.as_str(), to.as_str())
        {
            self.current_prefix = ResourceIdentity::new(rewritten);
        }
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

    pub(crate) fn direct_origin_mut(&mut self) -> Option<&mut LoopElementOrigin> {
        self.direct.as_deref_mut()
    }

    #[cfg(test)]
    pub(crate) fn module_prefixes(&self) -> &[LoopModulePrefixOrigin] {
        &self.module_prefixes
    }

    pub(crate) fn module_prefixes_mut(&mut self) -> &mut [LoopModulePrefixOrigin] {
        &mut self.module_prefixes
    }

    pub(crate) fn push_module_prefix(&mut self, origin: LoopModulePrefixOrigin) {
        self.module_prefixes.push(origin);
    }

    pub(crate) fn set_display_file(&mut self, file: &str) {
        if let Some(origin) = &mut self.direct {
            origin.set_display_file(file);
        }
        for prefix in &mut self.module_prefixes {
            prefix.origin_mut().set_display_file(file);
        }
    }

    pub(crate) fn legacy_candidates(
        &self,
        current_identity: &ResourceIdentity,
    ) -> Vec<(LoopElementOrigin, LegacyLoopAddress)> {
        let mut candidates = Vec::new();
        if let Some(origin) = &self.direct {
            candidates.push((origin.as_ref().clone(), origin.legacy_address().clone()));
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
                candidates.push((prefix.origin().clone(), LegacyLoopAddress::new(legacy)));
            }
        }

        candidates.dedup_by(|left, right| left.1 == right.1);
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

    pub fn file(&self) -> &str {
        self.source.file()
    }

    pub fn line(&self) -> usize {
        self.source.line()
    }

    pub fn column(&self) -> usize {
        self.source.column()
    }

    pub(crate) fn scheduler_key(&self) -> &str {
        self.source.scheduler_key()
    }

    pub(crate) fn same_system_target(&self, other: &Self) -> bool {
        self.source.same_system_target(&other.source)
    }

    pub(crate) fn element_origin(&self, element: ForLoopElement) -> LoopElementOrigin {
        let legacy = match &element {
            ForLoopElement::Index(index) => {
                LegacyLoopAddress::new(format!("{}[{index}]", self.legacy_base.0))
            }
            ForLoopElement::Key(key) => LegacyLoopAddress::new(crate::utils::map_key_address(
                self.legacy_base.exact_state_name(),
                key,
            )),
        };
        LoopElementOrigin::new(LoopOriginKind::For { element }, self.source.clone(), legacy)
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

    #[cfg(test)]
    pub(crate) fn legacy_base(&self) -> &LegacyLoopAddress {
        &self.legacy_base
    }

    pub(crate) fn prefix_for_module(&mut self, prefix: &ResourceIdentity) {
        self.source.scope_to_module(prefix);
        self.legacy_base = self.legacy_base.with_parent_prefix(prefix);
    }
}

pub(crate) fn replace_identity_prefix(value: &str, from: &str, to: &str) -> Option<String> {
    let suffix = value.strip_prefix(from)?;
    if !suffix.is_empty() && !suffix.starts_with('.') {
        return None;
    }
    Some(format!("{to}{suffix}"))
}
