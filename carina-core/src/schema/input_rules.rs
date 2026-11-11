//! Portable desired-input declarations. Scope, condition and target are required schema
//! data; only classified desired write sites can evaluate these declarations.

use std::{collections::HashSet, fmt};

use crate::provider_text::{DiagnosticQuote, DiagnosticTextBuilder};

use super::{
    AttrTypeKind, Compiled, CompiledPattern, Draft, PatternAttributeType, PatternForm,
    PatternResourceSchema, SchemaDefinitionIssue, SchemaValidationIssueLocation, TypeIdentity,
};

pub(crate) const MAX_INPUT_CONDITION_DEPTH: usize = 16;

/// A malformed input declaration, independent of any input value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InputRuleError {
    #[error("input input path must be a dotted sequence of attribute identifiers")]
    InvalidPath,
    #[error("input rule code must contain lowercase identifier segments separated by dots")]
    InvalidCode,
    #[error("forbidden input token must not be empty")]
    EmptyToken,
    #[error("input character set must not be empty")]
    EmptyCharSet,
    #[error("input character range has inverted endpoints")]
    InvertedCharRange,
    #[error("input character range endpoint is not a Unicode scalar")]
    InvalidUnicodeScalar,
    #[error("input length must have at least one bound")]
    EmptyLengthBounds,
    #[error("input length minimum exceeds maximum")]
    InvertedLengthBounds,
    #[error("input input path does not name a canonical attribute or struct field")]
    UnknownPath,
    #[error("input input path is provider-populated")]
    ProviderPopulatedPath,
    #[error("input input path must have a String-based type")]
    NonStringPath,
    #[error("input map-key target must name a map with String-based keys")]
    NonStringKeyMapPath,
    #[error("duplicate input rule target and code")]
    DuplicateRule,
    #[error("input literal does not match the declared scalar type or refinement")]
    LiteralTypeMismatch,
    #[error("input enum literal has a different type identity")]
    EnumIdentityMismatch,
    #[error("input enum literal must be a declared canonical API value")]
    InvalidEnumLiteral,
    #[error("input equality requires a supported scalar declaration")]
    UnsupportedLiteral,
    #[error("input condition exceeds the maximum depth of 16")]
    ConditionTooDeep,
    #[error("map-key rules require a string predicate")]
    IncompatiblePredicate,
}

/// A nonempty union of inclusive Unicode scalar ranges, sorted and coalesced.
/// Surrogates are unrepresentable because endpoints are Rust `char`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharSet(Vec<(char, char)>);

impl CharSet {
    pub fn new(ranges: impl IntoIterator<Item = (char, char)>) -> Result<Self, InputRuleError> {
        let mut ranges: Vec<_> = ranges.into_iter().collect();
        if ranges.is_empty() {
            return Err(InputRuleError::EmptyCharSet);
        }
        if ranges.iter().any(|(start, end)| start > end) {
            return Err(InputRuleError::InvertedCharRange);
        }
        ranges.sort_unstable();
        let mut canonical: Vec<(char, char)> = Vec::new();
        for (start, end) in ranges {
            if let Some((_, previous_end)) = canonical.last_mut() {
                // U+D800..U+DFFF contains no scalars, so U+D7FF and U+E000
                // are adjacent in this set's domain as well.
                let adjacent = if *previous_end == '\u{d7ff}' {
                    0xe000
                } else {
                    u32::from(*previous_end) + 1
                };
                if u32::from(start) <= adjacent {
                    *previous_end = (*previous_end).max(end);
                    continue;
                }
            }
            canonical.push((start, end));
        }
        Ok(Self(canonical))
    }

    pub fn ranges(&self) -> &[(char, char)] {
        &self.0
    }

    pub fn contains(&self, value: char) -> bool {
        let index = self.0.partition_point(|(start, _)| *start <= value);
        index > 0 && value <= self.0[index - 1].1
    }

    pub fn ascii_lowercase() -> Self {
        Self(vec![('a', 'z')])
    }
    pub fn ascii_uppercase() -> Self {
        Self(vec![('A', 'Z')])
    }
    pub fn ascii_digits() -> Self {
        Self(vec![('0', '9')])
    }
    pub fn ascii_letters() -> Self {
        Self(vec![('A', 'Z'), ('a', 'z')])
    }
    pub fn ascii_alphanumeric() -> Self {
        Self(vec![('0', '9'), ('A', 'Z'), ('a', 'z')])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmptyString(String);

impl NonEmptyString {
    pub fn new(value: impl Into<String>) -> Result<Self, InputRuleError> {
        let value = value.into();
        if value.is_empty() {
            Err(InputRuleError::EmptyToken)
        } else {
            Ok(Self(value))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A stable diagnostic identifier, with `[a-z][a-z0-9_]*` dot-separated segments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputRuleCode(String);

impl InputRuleCode {
    pub fn new(value: impl Into<String>) -> Result<Self, InputRuleError> {
        let value = value.into();
        if value.split('.').all(|segment| {
            segment
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_lowercase)
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        }) {
            Ok(Self(value))
        } else {
            Err(InputRuleError::InvalidCode)
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InputRuleCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A canonical dotted attribute/struct-field path, without block aliases or
/// collection wildcards. Schema admission proves it names a compatible writable node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputPath(String);

impl InputPath {
    pub fn new(value: impl Into<String>) -> Result<Self, InputRuleError> {
        let value = value.into();
        if value.split('.').all(super::is_path_identifier) {
            Ok(Self(value))
        } else {
            Err(InputRuleError::InvalidPath)
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn segments(&self) -> std::str::Split<'_, char> {
        self.0.split('.')
    }
    pub(crate) fn root(&self) -> &str {
        self.segments()
            .next()
            .expect("validated nonempty input path")
    }
}

impl fmt::Display for InputPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = DiagnosticTextBuilder::default();
        text.push_declaration_path(self.as_str());
        fmt::Display::fmt(&text.finish(), f)
    }
}

/// A scalar declaration value, never a deferred or provider-observed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputLiteral {
    Bool(bool),
    Int(i64),
    Float(crate::FiniteF64),
    String(String),
    Enum {
        identity: TypeIdentity,
        api_value: String,
    },
}

/// One guard on an existing input rule. Admission bounds nesting before use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputCondition {
    Always,
    Present(InputPath),
    Equals {
        path: InputPath,
        literal: InputLiteral,
    },
    Not(Box<InputCondition>),
}

impl InputCondition {
    pub(crate) fn path(&self) -> Option<&InputPath> {
        let mut condition = self;
        loop {
            match condition {
                Self::Always => return None,
                Self::Present(path) | Self::Equals { path, literal: _ } => return Some(path),
                Self::Not(inner) => condition = inner,
            }
        }
    }
}

impl InputLiteral {
    pub(super) fn push_to(&self, text: &mut DiagnosticTextBuilder) {
        match self {
            Self::Bool(value) => text.push_str(&value.to_string()),
            Self::Int(value) => text.push_str(&value.to_string()),
            Self::Float(value) => text.push_str(&value.to_string()),
            Self::String(value) => text.push_quoted_str(value, DiagnosticQuote::Double),
            Self::Enum {
                identity,
                api_value,
            } => {
                text.push_declaration_path(&identity.to_string());
                text.push_str("::");
                text.push_quoted_str(api_value, DiagnosticQuote::Double);
            }
        }
    }
}

impl fmt::Display for InputLiteral {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = DiagnosticTextBuilder::default();
        self.push_to(&mut text);
        fmt::Display::fmt(&text.finish(), f)
    }
}

impl InputCondition {
    pub(super) fn push_to(&self, text: &mut DiagnosticTextBuilder) {
        match self {
            Self::Always => text.push_str("always"),
            Self::Present(path) => {
                text.push_declaration_path(path.as_str());
                text.push_str(" is present");
            }
            Self::Equals { path, literal } => {
                text.push_declaration_path(path.as_str());
                text.push_str(" equals ");
                literal.push_to(text);
            }
            Self::Not(inner) => {
                text.push_str("not (");
                inner.push_to(text);
                text.push_str(")");
            }
        }
    }
}

impl fmt::Display for InputCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = DiagnosticTextBuilder::default();
        self.push_to(&mut text);
        fmt::Display::fmt(&text.finish(), f)
    }
}

/// An instantiated diagnostic location; declaration paths remain wildcard-free.
#[derive(Debug, Clone)]
pub struct InputDiagnosticPath(Box<InputDiagnosticPathData>);

#[derive(Debug, Clone)]
pub(crate) enum InputDiagnosticKey {
    Public(String),
    // The ordinal is local to its structured owner prefix, not a rendering
    // of the secret key. No plaintext leaves the protected evidence graph.
    Protected { ordinal: usize },
}

pub(crate) enum InputDiagnosticSegment {
    Field(String),
    Index(usize),
    Key(InputDiagnosticKey),
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum DiagnosticIdentity<'a> {
    Field(&'a str),
    Index(usize),
    PublicKey(&'a crate::resource::ValueString),
    ProtectedKey(usize),
}

#[derive(Debug, Clone)]
struct InputDiagnosticPathData {
    rendered: String,
    root: String,
    relative: super::ConcreteFieldPath,
    protected: Box<[(usize, usize)]>,
}
impl InputDiagnosticPath {
    #[cfg(test)]
    pub(crate) fn new(root: String, relative: super::FieldPath) -> Self {
        Self::from_segments(
            root,
            relative
                .canonical()
                .segments()
                .iter()
                .map(|segment| match segment {
                    super::LeafPathSegment::StructField(name) => {
                        InputDiagnosticSegment::Field(name.clone())
                    }
                    super::LeafPathSegment::ListIndex(index) => {
                        InputDiagnosticSegment::Index(*index)
                    }
                    super::LeafPathSegment::MapKey(key) => {
                        InputDiagnosticSegment::Key(InputDiagnosticKey::Public(
                            key.diagnostic_literal().expect("public fixture key").into(),
                        ))
                    }
                }),
        )
    }
    pub(crate) fn from_segments(
        root: String,
        segments: impl IntoIterator<Item = InputDiagnosticSegment>,
    ) -> Self {
        use std::fmt::Write;
        let mut rendered = String::new();
        let mut relative = super::FieldPath::new();
        let mut protected = Vec::new();
        super::write_path_field(&mut rendered, &root, false).expect("write String");
        for (position, segment) in segments.into_iter().enumerate() {
            match segment {
                InputDiagnosticSegment::Field(name) => {
                    super::write_path_field(&mut rendered, &name, true).expect("write String");
                    relative = relative.push_field(name);
                }
                InputDiagnosticSegment::Index(index) => {
                    write!(rendered, "[{index}]").expect("write String");
                    relative = relative.push_index(index);
                }
                InputDiagnosticSegment::Key(InputDiagnosticKey::Public(key)) => {
                    write!(rendered, "[{key:?}]").expect("write String");
                    relative = relative.push_map_key(key);
                }
                InputDiagnosticSegment::Key(InputDiagnosticKey::Protected { ordinal }) => {
                    rendered.push_str("[\"<secret>\"]");
                    protected.push((position, ordinal));
                    relative = relative.push_protected_map_key();
                }
            }
        }
        Self(Box::new(InputDiagnosticPathData {
            rendered,
            root,
            relative: relative.into_concrete(),
            protected: protected.into_boxed_slice(),
        }))
    }
    pub fn as_str(&self) -> &str {
        &self.0.rendered
    }
    pub fn root(&self) -> &str {
        &self.0.root
    }
    pub fn relative(&self) -> super::FieldPath {
        self.0.relative.as_field_path()
    }
    fn identity(&self) -> impl Iterator<Item = DiagnosticIdentity<'_>> {
        let mut next_protected = 0;
        self.0
            .relative
            .canonical()
            .iter()
            .enumerate()
            .map(move |(position, segment)| {
                if let Some(&(index, ordinal)) = self.0.protected.get(next_protected)
                    && index == position
                {
                    next_protected += 1;
                    return DiagnosticIdentity::ProtectedKey(ordinal);
                }
                match segment {
                    super::LeafPathSegment::StructField(name) => DiagnosticIdentity::Field(name),
                    super::LeafPathSegment::ListIndex(index) => DiagnosticIdentity::Index(*index),
                    super::LeafPathSegment::MapKey(key) => DiagnosticIdentity::PublicKey(key),
                }
            })
    }
}
impl PartialEq for InputDiagnosticPath {
    fn eq(&self, other: &Self) -> bool {
        self.0.root == other.0.root && self.identity().eq(other.identity())
    }
}
impl Eq for InputDiagnosticPath {}
impl PartialOrd for InputDiagnosticPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for InputDiagnosticPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .root
            .cmp(&other.0.root)
            .then_with(|| self.identity().cmp(other.identity()))
    }
}
impl fmt::Display for InputDiagnosticPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            crate::provider_text::DiagnosticText::new(&self.0.rendered)
        )
    }
}
impl From<InputPath> for InputDiagnosticPath {
    fn from(path: InputPath) -> Self {
        let mut parts = path.segments();
        let root = parts.next().expect("admitted path has a root").to_owned();
        Self::from_segments(
            root,
            parts.map(|part| InputDiagnosticSegment::Field(part.into())),
        )
    }
}

/// Each declaration is one conjunct. Length counts Unicode scalar values,
/// matching ordinary String refinements. Empty strings satisfy AllowedChars
/// and NoAdjacent, but fail FirstChar and LastChar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPredicate<P> {
    Present,
    Absent,
    Equals(InputLiteral),
    Length {
        min: Option<u64>,
        max: Option<u64>,
    },
    AllowedChars(CharSet),
    FirstChar(CharSet),
    LastChar(CharSet),
    NoSubstring(NonEmptyString),
    NoPrefix(NonEmptyString),
    ForbiddenPrefix {
        prefix: NonEmptyString,
        ignore_ascii_case: bool,
    },
    NoSuffix(NonEmptyString),
    NoAdjacent {
        left: CharSet,
        right: CharSet,
    },
    NotIpv4,
    Matches(P),
    NotMatches(P),
}

impl<P> InputPredicate<P> {
    pub(crate) fn violation_reason(&self) -> InputViolationReason {
        match self {
            Self::Present => InputViolationReason::Present,
            Self::Absent => InputViolationReason::Absent,
            Self::Equals(literal) => InputViolationReason::Equals(literal.clone()),
            Self::Length { min: _, max: _ } => InputViolationReason::Length,
            Self::AllowedChars(_) => InputViolationReason::AllowedChars,
            Self::FirstChar(_) => InputViolationReason::FirstChar,
            Self::LastChar(_) => InputViolationReason::LastChar,
            Self::NoSubstring(_) => InputViolationReason::NoSubstring,
            Self::NoPrefix(_) => InputViolationReason::NoPrefix,
            Self::ForbiddenPrefix {
                prefix: _,
                ignore_ascii_case: _,
            } => InputViolationReason::ForbiddenPrefix,
            Self::NoSuffix(_) => InputViolationReason::NoSuffix,
            Self::NoAdjacent { left: _, right: _ } => InputViolationReason::NoAdjacent,
            Self::NotIpv4 => InputViolationReason::NotIpv4,
            Self::Matches(_) => InputViolationReason::Matches,
            Self::NotMatches(_) => InputViolationReason::NotMatches,
        }
    }
}

/// The closed set of write scopes. Adding a scope requires updating the
/// exhaustive applicability decision in the write-site classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InputRuleScope {
    CreateOnly,
    EveryWrite,
}

/// What the predicate inspects, independent of the attribute's nominal type.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InputRuleTarget {
    Value(InputPath),
    MapKeys(InputPath),
}

impl InputRuleTarget {
    pub fn path(&self) -> &InputPath {
        match self {
            Self::Value(path) | Self::MapKeys(path) => path,
        }
    }
}

/// Schema declarations cannot evaluate arbitrary desired or observed strings.
/// Evaluation requires an applicable rule obtained from a classified write site.
///
/// ```compile_fail,E0599
/// use carina_core::schema::{Compiled, InputRule};
/// fn bypass(rule: &InputRule<Compiled>, observed: &str) {
///     let _ = rule.evaluate(observed);
/// }
/// ```
/// ```compile_fail,E0004
/// use carina_core::schema::InputRuleScope;
/// fn ignores_scope(scope: InputRuleScope) -> bool {
///     match scope { InputRuleScope::CreateOnly => true }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRule<R: PatternForm = Draft> {
    scope: InputRuleScope,
    target: InputRuleTarget,
    code: InputRuleCode,
    condition: InputCondition,
    predicate: InputPredicate<R::Pattern>,
}

/// Exhaustive read-only declaration view for transport and fingerprinting.
#[derive(Debug, Clone, Copy)]
pub struct InputRuleParts<'a, R: PatternForm = Draft> {
    pub scope: InputRuleScope,
    pub target: &'a InputRuleTarget,
    pub code: &'a InputRuleCode,
    pub condition: &'a InputCondition,
    pub predicate: &'a InputPredicate<R::Pattern>,
}

impl<R: PatternForm> InputRule<R> {
    pub fn new(
        scope: InputRuleScope,
        target: InputRuleTarget,
        code: InputRuleCode,
        predicate: InputPredicate<R::Pattern>,
    ) -> Self {
        Self {
            scope,
            target,
            code,
            condition: InputCondition::Always,
            predicate,
        }
    }
    pub fn value(
        scope: InputRuleScope,
        path: InputPath,
        code: InputRuleCode,
        predicate: InputPredicate<R::Pattern>,
    ) -> Self {
        Self::new(scope, InputRuleTarget::Value(path), code, predicate)
    }
    pub fn parts(&self) -> InputRuleParts<'_, R> {
        InputRuleParts {
            scope: self.scope,
            target: &self.target,
            code: &self.code,
            condition: &self.condition,
            predicate: &self.predicate,
        }
    }
    pub fn with_condition(mut self, condition: InputCondition) -> Self {
        self.condition = condition;
        self
    }
    pub fn condition(&self) -> &InputCondition {
        &self.condition
    }
    pub fn path(&self) -> &InputPath {
        self.target.path()
    }
    pub fn scope(&self) -> InputRuleScope {
        self.scope
    }
    pub fn target(&self) -> &InputRuleTarget {
        &self.target
    }
    pub fn code(&self) -> &InputRuleCode {
        &self.code
    }
    pub fn predicate(&self) -> &InputPredicate<R::Pattern> {
        &self.predicate
    }
}

/// A safe reason that never retains the rejected input string.
///
/// Reasons classify failures; only a violation can render its declaration.
///
/// ```compile_fail,E0277
/// use carina_core::schema::InputViolationReason;
/// let _ = format!("{}", InputViolationReason::NoSuffix);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputViolationReason {
    Present,
    Absent,
    Equals(InputLiteral),
    EvaluationFailed,
    Length,
    AllowedChars,
    FirstChar,
    LastChar,
    NoSubstring,
    NoPrefix,
    ForbiddenPrefix,
    NoSuffix,
    NoAdjacent,
    NotIpv4,
    Matches,
    NotMatches,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputViolationKind {
    PredicateRejected,
    EvaluationFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path} [{code}]: {message}", code = self.code(), message = self.message())]
pub struct InputViolation {
    pub(crate) path: InputDiagnosticPath,
    rule: Box<InputRule<Compiled>>,
    kind: InputViolationKind,
}

impl InputViolation {
    pub(crate) fn rejected(rule: &InputRule<Compiled>) -> Self {
        Self::new(rule, InputViolationKind::PredicateRejected)
    }

    pub(crate) fn evaluation_failed(rule: &InputRule<Compiled>) -> Self {
        Self::new(rule, InputViolationKind::EvaluationFailed)
    }

    fn new(rule: &InputRule<Compiled>, kind: InputViolationKind) -> Self {
        Self {
            path: rule.path().clone().into(),
            rule: Box::new(rule.clone()),
            kind,
        }
    }

    pub fn path(&self) -> &InputDiagnosticPath {
        &self.path
    }
    pub fn code(&self) -> &InputRuleCode {
        self.rule.code()
    }
    pub fn reason(&self) -> InputViolationReason {
        match self.kind {
            InputViolationKind::PredicateRejected => self.rule.predicate().violation_reason(),
            InputViolationKind::EvaluationFailed => InputViolationReason::EvaluationFailed,
        }
    }
    pub fn message(&self) -> InputViolationMessage<'_> {
        InputViolationMessage(self)
    }
}
pub struct InputViolationMessage<'a>(&'a InputViolation);
impl fmt::Display for InputViolationMessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.kind {
            InputViolationKind::PredicateRejected => super::InputRuleDisplay::new(&self.0.rule)
                .requirement()
                .fmt(f),
            InputViolationKind::EvaluationFailed => {
                super::input_rule_display::evaluation_failure(self.0.rule.condition()).fmt(f)
            }
        }
    }
}

impl InputRule<Draft> {
    pub(super) fn compile_patterns(
        self,
        location: SchemaValidationIssueLocation,
        issues: &mut Vec<SchemaDefinitionIssue>,
    ) -> Option<InputRule<Compiled>> {
        let Self {
            scope,
            target,
            code,
            condition,
            predicate,
        } = self;
        let mut compile = |source: super::PatternSpec| match CompiledPattern::try_new(&source) {
            Ok(pattern) => Some(pattern),
            Err(error) => {
                let location = location.clone();
                issues.push(SchemaDefinitionIssue {
                    path: location.raw_path().into(),
                    location,
                    message: error.to_string().into(),
                    pattern_error: Some(error),
                    input_rule_error: None,
                });
                None
            }
        };
        let predicate = match predicate {
            InputPredicate::Present => InputPredicate::Present,
            InputPredicate::Absent => InputPredicate::Absent,
            InputPredicate::Equals(literal) => InputPredicate::Equals(literal),
            InputPredicate::Length { min, max } => InputPredicate::Length { min, max },
            InputPredicate::AllowedChars(set) => InputPredicate::AllowedChars(set),
            InputPredicate::FirstChar(set) => InputPredicate::FirstChar(set),
            InputPredicate::LastChar(set) => InputPredicate::LastChar(set),
            InputPredicate::NoSubstring(token) => InputPredicate::NoSubstring(token),
            InputPredicate::NoPrefix(token) => InputPredicate::NoPrefix(token),
            InputPredicate::ForbiddenPrefix {
                prefix,
                ignore_ascii_case,
            } => InputPredicate::ForbiddenPrefix {
                prefix,
                ignore_ascii_case,
            },
            InputPredicate::NoSuffix(token) => InputPredicate::NoSuffix(token),
            InputPredicate::NoAdjacent { left, right } => {
                InputPredicate::NoAdjacent { left, right }
            }
            InputPredicate::NotIpv4 => InputPredicate::NotIpv4,
            InputPredicate::Matches(pattern) => InputPredicate::Matches(compile(pattern)?),
            InputPredicate::NotMatches(pattern) => InputPredicate::NotMatches(compile(pattern)?),
        };
        Some(InputRule::new(scope, target, code, predicate).with_condition(condition))
    }
}

impl<R: PatternForm> PatternResourceSchema<R> {
    pub(super) fn validate_input_rules(&self, issues: &mut Vec<SchemaDefinitionIssue>) {
        validate_rule_collection(
            &self.input_rules,
            |name| {
                self.attributes
                    .get(name)
                    .map(|a| (&a.attr_type, a.input_mode.is_provider_populated()))
            },
            &self.defs,
            SchemaValidationIssueLocation::InputRule,
            issues,
        );
    }
}

/// The same admission gate for resource and Struct owners (including defs).
pub(super) fn validate_rule_collection<'a, R: PatternForm>(
    rules: &[InputRule<R>],
    root: impl Fn(&str) -> Option<(&'a PatternAttributeType<R>, bool)>,
    defs: &'a std::collections::BTreeMap<String, PatternAttributeType<R>>,
    location: impl Fn(usize) -> SchemaValidationIssueLocation,
    issues: &mut Vec<SchemaDefinitionIssue>,
) {
    let resolve = |path: &InputPath| {
        let mut segments = path.segments();
        let (mut ty, mut populated) = root(segments.next().expect("nonempty input path"))
            .ok_or(InputRuleError::UnknownPath)?;
        for segment in segments {
            ty = peel_refs(ty, defs).ok_or(InputRuleError::UnknownPath)?;
            let AttrTypeKind::Struct { fields, .. } = &ty.kind else {
                return Err(InputRuleError::UnknownPath);
            };
            let field = fields.field(segment).ok_or(InputRuleError::UnknownPath)?;
            populated |= field.declared_input_mode.is_provider_populated();
            ty = &field.field_type;
        }
        if populated {
            return Err(InputRuleError::ProviderPopulatedPath);
        }
        peel_refs(ty, defs).ok_or(InputRuleError::UnknownPath)
    };
    let mut seen = HashSet::new();
    for (index, rule) in rules.iter().enumerate() {
        let mut reject = |error: InputRuleError| {
            let location = location(index);
            issues.push(SchemaDefinitionIssue {
                path: location.raw_path().into(),
                location,
                message: error.to_string().into(),
                pattern_error: None,
                input_rule_error: Some(error),
            });
        };
        if !seen.insert((rule.target(), rule.code())) {
            reject(InputRuleError::DuplicateRule);
        }
        if let InputPredicate::Length { min, max } = rule.predicate() {
            match (min, max) {
                (None, None) => reject(InputRuleError::EmptyLengthBounds),
                (Some(min), Some(max)) if min > max => reject(InputRuleError::InvertedLengthBounds),
                (Some(_), Some(_)) | (None, Some(_)) | (Some(_), None) => {}
            }
        }
        let check_target = || {
            let ty = resolve(rule.path())?;
            match rule.target() {
                InputRuleTarget::Value(_) => match rule.predicate() {
                    InputPredicate::Present | InputPredicate::Absent => Ok(()),
                    InputPredicate::Equals(literal) => admit_literal(ty, literal, defs),
                    _ => {
                        if string_based(ty, defs, &mut HashSet::new()) {
                            Ok(())
                        } else {
                            Err(InputRuleError::NonStringPath)
                        }
                    }
                },
                InputRuleTarget::MapKeys(_) => {
                    if !matches!(&ty.kind, AttrTypeKind::Map { key, .. } if string_based(key, defs, &mut HashSet::new()))
                    {
                        return Err(InputRuleError::NonStringKeyMapPath);
                    }
                    match rule.predicate() {
                        InputPredicate::Present
                        | InputPredicate::Absent
                        | InputPredicate::Equals(_) => Err(InputRuleError::IncompatiblePredicate),
                        _ => Ok(()),
                    }
                }
            }
        };
        if let Err(error) = check_target() {
            reject(error);
        }
        let mut condition = rule.condition();
        let mut depth = 1;
        while let InputCondition::Not(inner) = condition {
            depth += 1;
            if depth > MAX_INPUT_CONDITION_DEPTH {
                break;
            }
            condition = inner;
        }
        if depth > MAX_INPUT_CONDITION_DEPTH {
            reject(InputRuleError::ConditionTooDeep);
            continue;
        }
        let result = match condition {
            InputCondition::Always => Ok(()),
            InputCondition::Present(path) => resolve(path).map(|_| ()),
            InputCondition::Equals { path, literal } => {
                resolve(path).and_then(|ty| admit_literal(ty, literal, defs))
            }
            InputCondition::Not(_) => unreachable!("bounded Not chain was peeled"),
        };
        if let Err(error) = result {
            reject(error);
        }
    }
}

fn admit_literal<R: PatternForm>(
    ty: &PatternAttributeType<R>,
    literal: &InputLiteral,
    defs: &std::collections::BTreeMap<String, PatternAttributeType<R>>,
) -> Result<(), InputRuleError> {
    let ty = peel_refs(ty, defs).ok_or(InputRuleError::UnknownPath)?;
    let matches = match (&ty.kind, literal) {
        (AttrTypeKind::Bool, InputLiteral::Bool(_)) => true,
        (
            AttrTypeKind::Int {
                identity_layer: crate::schema::IntIdentityLayer { binding, range },
                member_layer:
                    crate::schema::IntMemberConstraints {
                        range: member_range,
                    },
            },
            InputLiteral::Int(value),
        ) => super::validate_scalar_refinement(
            binding,
            crate::resource::ConcreteValueRef::<crate::resource::NoPlaceholder>::Int(*value),
            || {
                super::validate_int_range(*range, *value)?;
                super::validate_int_range(*member_range, *value)
            },
        )
        .is_ok(),
        (AttrTypeKind::Float { binding, range }, InputLiteral::Float(value)) => {
            super::validate_scalar_refinement(
                binding,
                crate::resource::ConcreteValueRef::<crate::resource::NoPlaceholder>::Float(*value),
                || super::validate_float_range(*range, *value),
            )
            .is_ok()
        }
        (
            AttrTypeKind::String {
                identity_layer:
                    crate::schema::StringIdentityLayer {
                        binding,
                        pattern,
                        length,
                        to_dsl: _,
                    },
                member_layer:
                    crate::schema::StringMemberConstraints {
                        pattern: member_pattern,
                        length: member_length,
                    },
            },
            InputLiteral::String(value),
        ) => {
            let pattern = pattern
                .as_ref()
                .map(|pattern| CompiledPattern::try_new(pattern.as_ref()))
                .transpose()
                .map_err(|_| InputRuleError::LiteralTypeMismatch)?;
            let member_pattern = member_pattern
                .as_ref()
                .map(|pattern| CompiledPattern::try_new(pattern.as_ref()))
                .transpose()
                .map_err(|_| InputRuleError::LiteralTypeMismatch)?;
            let value = crate::resource::ValueString::from(value.clone());
            super::validate_scalar_refinement(
                binding,
                crate::resource::ConcreteValueRef::<crate::resource::NoPlaceholder>::String(&value),
                || {
                    super::validate_string_layers(
                        binding,
                        pattern.as_ref(),
                        *length,
                        member_pattern.as_ref(),
                        *member_length,
                        &value,
                    )
                },
            )
            .is_ok()
        }
        (
            AttrTypeKind::Reference {
                identity,
                pattern,
                length,
            },
            InputLiteral::String(value),
        ) => {
            let pattern = pattern
                .as_ref()
                .map(|pattern| CompiledPattern::try_new(pattern.as_ref()))
                .transpose()
                .map_err(|_| InputRuleError::LiteralTypeMismatch)?;
            super::validate_string_refinement(
                Some(identity),
                pattern.as_ref(),
                *length,
                &value.clone().into(),
            )
            .is_ok()
        }
        (AttrTypeKind::StringLiteralSet { values }, InputLiteral::String(value)) => {
            values.contains(value)
        }
        (
            AttrTypeKind::Enum {
                identity, values, ..
            },
            InputLiteral::Enum {
                identity: expected,
                api_value,
            },
        ) => {
            if identity != expected {
                return Err(InputRuleError::EnumIdentityMismatch);
            }
            if !values
                .as_ref()
                .is_some_and(|values| values.contains(api_value))
            {
                return Err(InputRuleError::InvalidEnumLiteral);
            }
            true
        }
        (
            AttrTypeKind::Bool
            | AttrTypeKind::Int { .. }
            | AttrTypeKind::Float { .. }
            | AttrTypeKind::String { .. }
            | AttrTypeKind::Reference { .. }
            | AttrTypeKind::StringLiteralSet { .. }
            | AttrTypeKind::Enum { .. },
            _,
        ) => false,
        (
            AttrTypeKind::UnloadedSchemaType(_)
            | AttrTypeKind::ResourceHandle { .. }
            | AttrTypeKind::Duration
            | AttrTypeKind::List { .. }
            | AttrTypeKind::Map { .. }
            | AttrTypeKind::Struct { .. }
            | AttrTypeKind::Union(_)
            | AttrTypeKind::Ref(_),
            _,
        ) => return Err(InputRuleError::UnsupportedLiteral),
    };
    if matches {
        Ok(())
    } else {
        Err(InputRuleError::LiteralTypeMismatch)
    }
}

fn peel_refs<'a, R: PatternForm>(
    mut ty: &'a PatternAttributeType<R>,
    defs: &'a std::collections::BTreeMap<String, PatternAttributeType<R>>,
) -> Option<&'a PatternAttributeType<R>> {
    let mut seen = HashSet::new();
    while let AttrTypeKind::Ref(name) = &ty.kind {
        if !seen.insert(name) {
            return None;
        }
        ty = defs.get(name)?;
    }
    Some(ty)
}

fn string_based<R: PatternForm>(
    ty: &PatternAttributeType<R>,
    defs: &std::collections::BTreeMap<String, PatternAttributeType<R>>,
    seen: &mut HashSet<String>,
) -> bool {
    match &ty.kind {
        AttrTypeKind::String { .. }
        | AttrTypeKind::Reference { .. }
        | AttrTypeKind::StringLiteralSet { .. } => true,
        AttrTypeKind::Enum { base, .. } => string_based(base, defs, seen),
        AttrTypeKind::Union(members) => {
            !members.as_raw_slice().is_empty()
                && members
                    .as_raw_slice()
                    .iter()
                    .all(|member| string_based(member, defs, seen))
        }
        AttrTypeKind::Ref(name) => {
            if !seen.insert(name.clone()) {
                return false;
            }
            let result = defs
                .get(name)
                .is_some_and(|ty| string_based(ty, defs, seen));
            seen.remove(name);
            result
        }
        AttrTypeKind::UnloadedSchemaType(_)
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::List { .. }
        | AttrTypeKind::Map { .. }
        | AttrTypeKind::Struct { .. } => false,
    }
}

#[cfg(test)]
mod scalar_literal_tests {
    use super::*;
    use crate::{
        parser::ResolvedSchemaType,
        resource::{ConcreteValue, Value},
        schema::{AttributeType, PatternMatch, PatternSpec, RefinementBinding},
    };

    #[test]
    fn literal_admission_checks_all_annotation_patterns() {
        let identity = TypeIdentity::new(Some("mock"), ["test"], "AnnotatedString");
        let declaration = |regex| {
            PatternAttributeType::<Compiled>::refined_string(
                Some(identity.clone()),
                Some(
                    CompiledPattern::try_new(&PatternSpec::new(regex, PatternMatch::Full)).unwrap(),
                ),
                None,
                None,
            )
        };
        let first =
            ResolvedSchemaType::from_schema_declaration("mock", &declaration("a.*")).unwrap();
        let second =
            ResolvedSchemaType::from_schema_declaration("mock", &declaration(".*z")).unwrap();
        let annotation = first.conjoin(&second).into_annotation();
        let annotated = AttributeType {
            kind: AttrTypeKind::String {
                identity_layer: crate::schema::StringIdentityLayer {
                    binding: RefinementBinding::Annotation(annotation),
                    pattern: None,
                    length: None,
                    to_dsl: None,
                },
                member_layer: Default::default(),
            },
        };
        for (text, valid) in [("ab", false), ("bz", false), ("az", true)] {
            assert_eq!(
                admit_literal(
                    &annotated,
                    &InputLiteral::String(text.into()),
                    &Default::default()
                )
                .is_ok(),
                valid
            );
            let compiled = annotated
                .clone()
                .compile_patterns(
                    SchemaValidationIssueLocation::Attribute("value".into()),
                    &mut vec![],
                )
                .unwrap();
            let value: Value = Value::Concrete(ConcreteValue::String(text.into()));
            assert_eq!(compiled.validate(&value).is_ok(), valid);
        }
    }

    #[test]
    fn literal_admission_rejects_empty_annotation_intersections() {
        let identity = TypeIdentity::new(Some("mock"), ["test"], "AnnotatedInt");
        let declaration = |min, max| {
            PatternAttributeType::<Compiled>::refined_int(
                Some(identity.clone()),
                Some((Some(min), Some(max))),
            )
        };
        let first =
            ResolvedSchemaType::from_schema_declaration("mock", &declaration(1, 2)).unwrap();
        let second =
            ResolvedSchemaType::from_schema_declaration("mock", &declaration(3, 4)).unwrap();
        let annotation = first.conjoin(&second).into_annotation();
        assert!(annotation.has_empty_intersection());
        let annotated = AttributeType {
            kind: AttrTypeKind::Int {
                identity_layer: crate::schema::IntIdentityLayer {
                    binding: RefinementBinding::Annotation(annotation),
                    range: None,
                },
                member_layer: Default::default(),
            },
        };
        assert_eq!(
            admit_literal(&annotated, &InputLiteral::Int(2), &Default::default()),
            Err(InputRuleError::LiteralTypeMismatch)
        );
        let compiled = annotated
            .compile_patterns(
                SchemaValidationIssueLocation::Attribute("value".into()),
                &mut vec![],
            )
            .unwrap();
        let value: Value = Value::Concrete(ConcreteValue::Int(2));
        assert!(compiled.validate(&value).is_err());
    }

    #[test]
    fn condition_literals_enforce_both_refinement_layers() {
        use crate::schema::{
            IntIdentityLayer, IntMemberConstraints, StringIdentityLayer, StringMemberConstraints,
        };
        let string = AttributeType::string_member(
            StringIdentityLayer::new(
                Some(TypeIdentity::bare("Name")),
                Some(PatternSpec::new("[a-z]+", PatternMatch::Full)),
                Some((Some(2), Some(100))),
                None,
            ),
            StringMemberConstraints::new(
                Some(PatternSpec::new("a.*", PatternMatch::Full)),
                Some((Some(2), Some(4))),
            ),
        );
        for (text, valid) in [
            ("az", true),
            ("zz", false),
            ("a", false),
            ("abbbb", false),
            ("AB", false),
        ] {
            assert_eq!(
                admit_literal(
                    &string,
                    &InputLiteral::String(text.into()),
                    &Default::default()
                )
                .is_ok(),
                valid,
                "{text}"
            );
        }
        let integer = AttributeType::int_member(
            IntIdentityLayer::new(
                Some(TypeIdentity::bare("Count")),
                Some((Some(1), Some(100))),
            ),
            IntMemberConstraints::new(Some((Some(10), Some(20)))),
        );
        for (value, valid) in [(5, false), (15, true), (25, false), (101, false)] {
            assert_eq!(
                admit_literal(&integer, &InputLiteral::Int(value), &Default::default()).is_ok(),
                valid,
                "{value}"
            );
        }
    }

    #[test]
    fn condition_literals_and_values_run_the_callback_once_after_the_member_layer() {
        use crate::schema::{StringIdentityLayer, StringMemberConstraints};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let ty = AttributeType::string_member(
            StringIdentityLayer::with_validator(
                TypeIdentity::bare("Name"),
                None,
                Some((Some(1), Some(100))),
                Arc::new(move |_| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
                None,
            ),
            StringMemberConstraints::new(None, Some((Some(1), Some(10)))),
        );
        let compiled = ty
            .clone()
            .compile_patterns(
                SchemaValidationIssueLocation::Attribute("name".into()),
                &mut vec![],
            )
            .unwrap();
        for (text, valid) in [("short".to_string(), true), ("a".repeat(50), false)] {
            calls.store(0, Ordering::SeqCst);
            assert_eq!(
                admit_literal(
                    &ty,
                    &InputLiteral::String(text.clone()),
                    &Default::default()
                )
                .is_ok(),
                valid
            );
            assert_eq!(calls.swap(0, Ordering::SeqCst), usize::from(valid));
            let value: Value = Value::Concrete(ConcreteValue::String(text.into()));
            assert_eq!(compiled.validate(&value).is_ok(), valid);
            assert_eq!(calls.swap(0, Ordering::SeqCst), usize::from(valid));
        }
    }
}
