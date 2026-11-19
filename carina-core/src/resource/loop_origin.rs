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

/// A child binding minted only by for-loop address admission.
///
/// Raw bindings and historical serialized addresses cannot grant admission.
///
/// ```compile_fail,E0451
/// use carina_core::resource::{BoundForAddress, RelativeBindingName};
/// let _ = BoundForAddress { binding: RelativeBindingName::new("rs[0]") };
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::resource::{BoundForAddress, RelativeBindingName};
/// let _: BoundForAddress<RelativeBindingName> = serde_json::from_str("\"rs[0]\"").unwrap();
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::resource::{BoundForAddress, RelativeBindingName};
/// let _: BoundForAddress<RelativeBindingName> = RelativeBindingName::new("rs[0]").into();
/// ```
#[derive(Debug, Clone)]
pub struct BoundForAddress<B: super::BindingAddress> {
    binding: B,
}

impl<B: super::BindingAddress> BoundForAddress<B> {
    pub(crate) fn binding_address(&self) -> &B {
        &self.binding
    }

    pub(crate) fn identity(&self) -> ResourceIdentity {
        ResourceIdentity::new(self.binding.as_str())
    }
}

/// Key material is obtainable only from an unflagged String classification.
struct ValueKey(String);

impl ValueKey {
    fn from_string(value: &super::ValueString) -> Result<Self, ForKeyError> {
        value
            .diagnostic_literal()
            .map(|key| Self(key.to_owned()))
            .ok_or(ForKeyError::Protected)
    }
}

/// Safe, core-owned diagnostics for value-key admission. Ordinals are one-based.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForKeyError {
    #[error("Element {element} must be a String. Rewrite as strings or use `for (i, x) in ...`.")]
    NonString { element: usize },
    #[error("Elements {first} and {second} produce the same key. Use `for (i, x) in ...`.")]
    Duplicate { first: usize, second: usize },
    #[error("Protected values cannot be value keys. Use `for (i, x) in ...`.")]
    Protected,
    #[error("Element {element} is not known before apply. Use `for (i, x) in ...`.")]
    NotKnownBeforeApply { element: usize },
    #[error("Element {element} is still unknown during expansion. Use `for (i, x) in ...`.")]
    PlanPending { element: usize },
    #[error("Element {element} is still unknown during expansion. Use `for (i, x) in ...`.")]
    UnknownDuringExpansion { element: usize },
    #[error(
        "Element type {kind} cannot be a value key. Rewrite as strings or use `for (i, x) in ...`."
    )]
    ElementType { kind: &'static str },
    #[error(
        "This loop is value-keyed; use a string subscript, or `for (i, x) in ...` for index addressing."
    )]
    NumericSubscript,
    #[error("expected {expected} iterable, got {got}")]
    Shape {
        expected: &'static str,
        got: &'static str,
    },
}

/// Unresolved nominal declarations retain their explicit lack of scalar evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForKeyEligibility {
    String,
    Unjudged,
}

/// Static eligibility is exhaustive: adding a TypeExpr must decide this contract.
pub(crate) fn classify_for_key_type(
    ty: &crate::parser::TypeExpr,
) -> Result<ForKeyEligibility, ForKeyError> {
    use crate::parser::TypeExpr;
    let reject = |kind| Err(ForKeyError::ElementType { kind });
    match ty {
        TypeExpr::String => Ok(ForKeyEligibility::String),
        TypeExpr::StringLiteral(_) => Ok(ForKeyEligibility::String),
        TypeExpr::Simple(name) => {
            if matches!(
                name.declaration().raw_shape(),
                crate::schema::RawShape::String { .. }
            ) {
                Ok(ForKeyEligibility::String)
            } else {
                reject("non-String refinement")
            }
        }
        TypeExpr::SchemaType { owned } => {
            if owned.scalar_kind() == crate::schema::RefinedScalarKind::String {
                Ok(ForKeyEligibility::String)
            } else {
                reject("non-String refinement")
            }
        }
        TypeExpr::Union(members) => {
            let mut result = ForKeyEligibility::String;
            for member in members {
                if classify_for_key_type(member)? == ForKeyEligibility::Unjudged {
                    result = ForKeyEligibility::Unjudged;
                }
            }
            Ok(result)
        }
        TypeExpr::UnloadedSchemaType(_) => Ok(ForKeyEligibility::Unjudged),
        TypeExpr::DottedUnresolved(_) => Ok(ForKeyEligibility::Unjudged),
        TypeExpr::Bool => reject("Bool"),
        TypeExpr::Int => reject("Int"),
        TypeExpr::Float => reject("Float"),
        TypeExpr::Duration => reject("Duration"),
        TypeExpr::List(_) => reject("List"),
        TypeExpr::Map(_) => reject("Map"),
        TypeExpr::Ref(_) => reject("Ref"),
        TypeExpr::Struct { .. } => reject("Struct"),
    }
}

/// Admit every child before returning any address. List order is retained;
/// map keys retain their existing sorted traversal and spelling.
pub(crate) fn admit_bound_for_addresses<P: super::PlaceholderKind, B: super::BindingAddress>(
    binding: &crate::parser::ForBinding,
    target: &crate::parser::BoundDeferredTarget<B>,
    members: super::knowledge::ClassifiedRef<'_, '_, P, B>,
) -> Result<Vec<BoundForAddress<B>>, ForKeyError> {
    use super::ConcreteValueRef;
    use super::knowledge::ClassifiedRef;
    use crate::parser::ForBinding;
    let base = target.binding_address();
    let got = match members.concrete() {
        Some(ConcreteValueRef::List(_) | ConcreteValueRef::StringList(_)) => "list",
        Some(ConcreteValueRef::Map(_)) => "map",
        Some(ConcreteValueRef::String(_)) => "string",
        Some(ConcreteValueRef::EnumIdentifier(_) | ConcreteValueRef::CanonicalEnum(_)) => "enum",
        Some(ConcreteValueRef::Int(_)) => "int",
        Some(ConcreteValueRef::Float(_)) => "float",
        Some(ConcreteValueRef::Bool(_)) => "bool",
        Some(ConcreteValueRef::Duration(_)) => "duration",
        None => "unresolved",
    };
    match binding {
        ForBinding::Simple(_) => {
            if members.is_protected() {
                return Err(ForKeyError::Protected);
            }
            let items = if let Some(items) = members.list() {
                items
                    .into_iter()
                    .enumerate()
                    .map(|(index, item)| {
                        let element = index + 1;
                        if item.is_protected() {
                            return Err(ForKeyError::Protected);
                        }
                        match item.concrete() {
                            Some(ConcreteValueRef::String(value)) => ValueKey::from_string(value),
                            Some(_) => Err(ForKeyError::NonString { element }),
                            None => Err(match item {
                                ClassifiedRef::Pending(pending) => match pending.phase() {
                                    super::knowledge::Phase::Plan => {
                                        ForKeyError::PlanPending { element }
                                    }
                                    super::knowledge::Phase::Apply => {
                                        ForKeyError::NotKnownBeforeApply { element }
                                    }
                                },
                                ClassifiedRef::Failed(_) => ForKeyError::PlanPending { element },
                                ClassifiedRef::Shape(_) | ClassifiedRef::Known(_) => {
                                    ForKeyError::NonString { element }
                                }
                            }),
                        }
                    })
                    .collect::<Vec<_>>()
            } else if let Some(ConcreteValueRef::StringList(strings)) = members.concrete() {
                strings.iter().map(ValueKey::from_string).collect()
            } else {
                return Err(ForKeyError::Shape {
                    expected: "list",
                    got,
                });
            };
            if items
                .iter()
                .any(|item| matches!(item, Err(ForKeyError::Protected)))
            {
                return Err(ForKeyError::Protected);
            }
            let mut seen = std::collections::BTreeMap::new();
            let mut keys = Vec::with_capacity(items.len());
            let mut pending = None;
            for (index, item) in items.into_iter().enumerate() {
                let element = index + 1;
                let key = match item {
                    Ok(key) => key,
                    Err(error @ ForKeyError::PlanPending { .. }) => {
                        if pending.is_none() {
                            pending = Some(error);
                        }
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if let Some(first) = seen.insert(key.0.clone(), element) {
                    return Err(ForKeyError::Duplicate {
                        first,
                        second: element,
                    });
                }
                keys.push(key);
            }
            if let Some(error) = pending {
                return Err(error);
            }
            Ok(keys
                .into_iter()
                .map(|key| BoundForAddress {
                    binding: super::binding_with_map_key(base, &key.0.into()),
                })
                .collect())
        }
        ForBinding::Indexed(_, _) => {
            let len = if let Some(items) = members.list() {
                items.len()
            } else if let Some(ConcreteValueRef::StringList(strings)) = members.concrete() {
                strings.len()
            } else {
                return Err(ForKeyError::Shape {
                    expected: "list",
                    got,
                });
            };
            Ok((0..len)
                .map(|index| BoundForAddress {
                    binding: super::binding_with_index(
                        base,
                        index
                            .try_into()
                            .expect("loop index fits in signed address domain"),
                    ),
                })
                .collect())
        }
        ForBinding::Map(_, _) => {
            let items = members.map().ok_or(ForKeyError::Shape {
                expected: "map",
                got,
            })?;
            let mut keys: Vec<_> = items.keys().copied().collect();
            keys.sort();
            Ok(keys
                .into_iter()
                .map(|key| BoundForAddress {
                    binding: super::binding_with_map_key(base, &key.into()),
                })
                .collect())
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
    file: crate::parser::SourceFileId,
    span: SourceSpan,
    source_text: Option<Arc<str>>,
    binding_scope: Arc<BTreeSet<String>>,
}

/// Source identity shared by every element produced from one unbound loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopSource {
    display_file: String,
    line: usize,
    column: usize,
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
    #[cfg(feature = "test-utils")]
    pub fn recording_parts(&self) -> (LoopKind, &ResourceIdentity) {
        (self.kind, &self.system_identity)
    }

    pub(crate) fn authored(
        source_file: &Path,
        span: SourceSpan,
        source_text: impl Into<crate::parser::retained::DiagnosticSourceText>,
        kind: LoopKind,
    ) -> Self {
        let display_file = source_file.display().to_string();
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
                file: crate::parser::SourceFileId::new(source_file.to_path_buf()),
                span,
                source_text: source_text.into().into_text(),
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

    pub(crate) fn scheduler_identity(&self) -> &ResourceIdentity {
        &self.system_identity
    }

    pub(crate) fn display_name(&self) -> String {
        format!("{}:{}", self.file(), self.line())
    }

    pub(crate) fn same_system_target(&self, other: &Self) -> bool {
        self.system_identity == other.system_identity
    }

    pub(crate) fn same_authored_source(&self, other: &Self) -> bool {
        match (self.source_span(), other.source_span()) {
            (Some(left), Some(right)) => {
                self.display_file == other.display_file
                    && left.start_byte() == right.start_byte()
                    && left.end_byte() == right.end_byte()
                    && self.kind == other.kind
            }
            (None, None) => self.same_system_target(other),
            (Some(_), None) | (None, Some(_)) => false,
        }
    }

    pub(crate) fn binding_scope_key(&self) -> String {
        Path::new(&self.display_file)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| ".".to_string(), |parent| parent.display().to_string())
    }

    pub(crate) fn source_text(&self) -> Option<&str> {
        self.authoring
            .as_ref()
            .and_then(|authoring| authoring.source_text.as_deref())
    }

    pub(crate) fn source_span(&self) -> Option<SourceSpan> {
        self.authoring.as_ref().map(|authoring| authoring.span)
    }

    pub(crate) fn source_file(&self) -> Option<&crate::parser::SourceFileId> {
        self.authoring.as_ref().map(|authoring| &authoring.file)
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

#[cfg(not(feature = "test-utils"))]
type LoopIdentityHasher = Sha256;
#[cfg(feature = "test-utils")]
type LoopIdentityHasher = crate::identity_recording::RecordingSha256;

fn loop_system_identity(
    file_or_parent_identity: &str,
    start: usize,
    end: usize,
    kind: LoopKind,
    module_scope: Option<&str>,
) -> ResourceIdentity {
    fn component(hasher: &mut LoopIdentityHasher, value: &[u8]) {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }

    let hasher = Sha256::new();
    #[cfg(not(feature = "test-utils"))]
    let mut hasher = hasher;
    #[cfg(feature = "test-utils")]
    let mut hasher =
        crate::identity_recording::RecordingSha256::new(hasher, "loop-system-identity");
    #[cfg(feature = "test-utils")]
    let recording = hasher.result_link();
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
    let identity = ResourceIdentity::new(format!("_loop_{}", hex::encode(&hasher.finalize()[..8])));
    #[cfg(feature = "test-utils")]
    if let Some(recording) = recording {
        recording.loop_result(&identity, module_scope.map(|_| file_or_parent_identity));
    }
    identity
}

/// Provenance for one concrete resource emitted by an unbound `for` or `if`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LoopElementOrigin {
    kind: LoopOriginKind,
    source: LoopSource,
    legacy: LegacyLoopAddress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_parent: Option<ResourceIdentity>,
}

impl LoopElementOrigin {
    #[cfg(feature = "test-utils")]
    pub fn recording_parts(&self) -> (&LoopSource, &str, Option<&ResourceIdentity>) {
        (
            &self.source,
            self.legacy.exact_state_name(),
            self.current_parent.as_ref(),
        )
    }

    pub(crate) fn new(kind: LoopOriginKind, source: LoopSource, legacy: LegacyLoopAddress) -> Self {
        Self {
            kind,
            source,
            legacy,
            current_parent: None,
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

    /// Authored span of the loop or conditional that produced this element.
    /// Deserialized historical provenance may not retain an authored span.
    pub fn source_span(&self) -> Option<SourceSpan> {
        self.source.source_span()
    }

    pub(crate) fn source_file(&self) -> Option<&crate::parser::SourceFileId> {
        self.source.source_file()
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

    fn prefix_current_parent(&mut self, prefix: &ResourceIdentity) {
        self.current_parent = Some(ResourceIdentity::new(
            self.current_parent.as_ref().map_or_else(
                || prefix.as_str().to_string(),
                |current| format!("{}.{}", prefix.as_str(), current.as_str()),
            ),
        ));
    }

    fn remap_identity_prefix(&mut self, from: &ResourceIdentity, to: &ResourceIdentity) {
        self.legacy.remap_identity_prefix(from, to);
        if let Some(current_parent) = &mut self.current_parent
            && let Some(rewritten) =
                replace_identity_prefix(current_parent.as_str(), from.as_str(), to.as_str())
        {
            *current_parent = ResourceIdentity::new(rewritten);
        }
    }

    pub(crate) fn set_binding_scope(&mut self, binding_scope: Arc<BTreeSet<String>>) {
        self.source.set_binding_scope(binding_scope);
    }

    pub(crate) fn rewritten_address(&self, binding: &str) -> String {
        let element_suffix = match &self.kind {
            LoopOriginKind::For { element } => element.address(""),
            LoopOriginKind::If => String::new(),
        };
        let parent = self
            .current_parent
            .as_ref()
            .map(|parent| format!("{}.", parent.as_str()))
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

    pub(crate) fn rewritten_resource_addresses(
        &self,
        current_identity: &ResourceIdentity,
        binding: &str,
    ) -> Option<(LegacyLoopAddress, String)> {
        let from = replace_identity_prefix(
            current_identity.as_str(),
            self.current_prefix.as_str(),
            self.legacy_prefix().exact_state_name(),
        )?;
        let bound_prefix = self.origin.rewritten_address(binding);
        let to = replace_identity_prefix(
            current_identity.as_str(),
            self.current_prefix.as_str(),
            &bound_prefix,
        )?;
        Some((LegacyLoopAddress::new(from), to))
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
        self.origin.prefix_current_parent(current_parent);
        self.origin.source_mut().scope_to_module(current_parent);
    }

    fn remap_identity_prefix(&mut self, from: &ResourceIdentity, to: &ResourceIdentity) {
        self.remap_current_prefix(from, to);
        self.origin.remap_identity_prefix(from, to);
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
    #[cfg(feature = "test-utils")]
    pub fn recording_parts(
        &self,
    ) -> (
        Option<&LoopElementOrigin>,
        impl Iterator<Item = (&LoopElementOrigin, &ResourceIdentity)>,
    ) {
        (
            self.direct_origin(),
            self.module_prefixes
                .iter()
                .map(|prefix| (prefix.origin(), prefix.current_prefix())),
        )
    }

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
            origin.prefix_current_parent(current_parent);
            origin.source_mut().scope_to_module(current_parent);
        }
        for prefix in &mut self.module_prefixes {
            prefix.prefix_for_module(current_parent, legacy_parent);
        }
    }

    pub(crate) fn remap_identity_prefix(&mut self, from: &ResourceIdentity, to: &ResourceIdentity) {
        if let Some(origin) = &mut self.direct {
            origin.remap_identity_prefix(from, to);
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

    pub(crate) fn scheduler_identity(&self) -> &ResourceIdentity {
        self.source.scheduler_identity()
    }

    pub(crate) fn display_name(&self) -> String {
        self.source.display_name()
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

    pub(crate) fn unbound_read_error(&self, kind: LoopKind) -> crate::parser::ParseError {
        crate::parser::ParseError::UnboundLoopRead {
            kind,
            line: self.source.line(),
            span: self.source.source_span(),
        }
    }

    pub(crate) fn matches_legacy_binding(&self, binding: &str) -> bool {
        super::decompose_binding_address(binding).is_some_and(|address| {
            address.is_self_or_descendant_of(self.legacy_base.exact_state_name())
        })
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

#[cfg(test)]
mod for_key_type_tests {
    use super::*;
    use crate::parser::{ResourceTypePath, TypeExpr};

    #[test]
    fn for_value_keys_failed_member_is_not_apply_evidence() {
        use crate::resource::{
            ConcreteValue, DeferredValue, NoPlaceholder, RelativeBindingName, Value,
        };
        let value: Value<NoPlaceholder, RelativeBindingName> = Value::Concrete(
            ConcreteValue::List(vec![Value::Deferred(DeferredValue::Unavailable {
                name: crate::builtins::BuiltinFunction::env,
                args: vec![],
                reason: crate::resource::UnavailableReason::from_builtin_error(
                    "unavailable".into(),
                ),
                source_line: 0.into(),
            })]),
        );
        let classified = value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .unwrap()
            .into_classification();
        let target =
            crate::parser::BoundDeferredTarget::new(RelativeBindingName::new("rs")).unwrap();
        let error = admit_bound_for_addresses(
            &crate::parser::ForBinding::Simple("x".into()),
            &target,
            crate::resource::knowledge::ClassifiedRef::from(&classified),
        )
        .unwrap_err();
        assert!(
            !matches!(error, ForKeyError::NotKnownBeforeApply { .. }),
            "a failed evaluation supplies no apply-phase evidence: {error}"
        );
    }

    #[test]
    fn for_value_keys_refinement_eligibility() {
        use crate::parser::{BuiltinTypeName, OwnedTypeIdentity, UnloadedSchemaType};
        use crate::schema::{RefinedScalarKind, TypeIdentity};
        assert_eq!(
            classify_for_key_type(&TypeExpr::Simple(
                BuiltinTypeName::lookup("ipv4_cidr").unwrap()
            )),
            Ok(ForKeyEligibility::String)
        );
        assert_eq!(
            classify_for_key_type(&TypeExpr::Simple(
                BuiltinTypeName::lookup("positive_int").unwrap()
            )),
            Err(ForKeyError::ElementType {
                kind: "non-String refinement"
            })
        );
        for (scalar, expected) in [
            (RefinedScalarKind::String, Ok(ForKeyEligibility::String)),
            (
                RefinedScalarKind::Int,
                Err(ForKeyError::ElementType {
                    kind: "non-String refinement",
                }),
            ),
            (
                RefinedScalarKind::Float,
                Err(ForKeyError::ElementType {
                    kind: "non-String refinement",
                }),
            ),
        ] {
            let owned = OwnedTypeIdentity::provider(
                "mock",
                TypeIdentity::new(Some("mock"), ["test"], "Refinement"),
                scalar,
            );
            assert_eq!(
                classify_for_key_type(&TypeExpr::SchemaType {
                    owned: owned.unbounded_witness_for_test()
                }),
                expected
            );
        }
        let unloaded =
            UnloadedSchemaType::try_from(TypeIdentity::new(Some("mock"), ["test"], "Unknown"))
                .unwrap();
        assert_eq!(
            classify_for_key_type(&TypeExpr::UnloadedSchemaType(unloaded)),
            Ok(ForKeyEligibility::Unjudged)
        );
    }

    #[test]
    fn for_value_keys_static_classifier() {
        for ty in [
            TypeExpr::String,
            TypeExpr::StringLiteral("TYPE_LITERAL_CANARY".into()),
            TypeExpr::Union(vec![
                TypeExpr::StringLiteral("a".into()),
                TypeExpr::StringLiteral("b".into()),
            ]),
        ] {
            assert_eq!(classify_for_key_type(&ty), Ok(ForKeyEligibility::String));
        }
        let unknown = TypeExpr::DottedUnresolved(ResourceTypePath::new("mock", "Unknown"));
        assert_eq!(
            classify_for_key_type(&unknown),
            Ok(ForKeyEligibility::Unjudged)
        );
        assert_eq!(
            classify_for_key_type(&TypeExpr::Union(vec![unknown.clone(), TypeExpr::String])),
            Ok(ForKeyEligibility::Unjudged)
        );
        let error = classify_for_key_type(&TypeExpr::Union(vec![
            unknown,
            TypeExpr::Bool,
            TypeExpr::StringLiteral("TYPE_LITERAL_CANARY".into()),
        ]))
        .unwrap_err();
        assert_eq!(error, ForKeyError::ElementType { kind: "Bool" });
        assert!(!error.to_string().contains("TYPE_LITERAL_CANARY"));
        assert_eq!(
            classify_for_key_type(&TypeExpr::Ref(ResourceTypePath::new("mock", "Resource"))),
            Err(ForKeyError::ElementType { kind: "Ref" })
        );
    }
}
