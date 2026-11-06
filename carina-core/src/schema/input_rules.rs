//! Portable desired-input declarations. Scope and target are required schema
//! data; only classified desired write sites can evaluate these declarations.

use std::{collections::HashSet, fmt};

use super::{
    AttrTypeKind, Compiled, CompiledPattern, Draft, PatternAttributeType, PatternForm,
    PatternResourceSchema, SchemaDefinitionIssue, SchemaValidationIssueLocation,
};

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
/// collection wildcards. Schema admission proves it names a writable string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputPath(String);

impl InputPath {
    pub fn new(value: impl Into<String>) -> Result<Self, InputRuleError> {
        let value = value.into();
        if value.split('.').all(|segment| {
            segment
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }) {
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
    pub(crate) fn attribute_segments(&self) -> Vec<String> {
        self.segments().map(str::to_owned).collect()
    }
    pub(crate) fn root(&self) -> &str {
        self.segments()
            .next()
            .expect("validated nonempty input path")
    }
}

impl fmt::Display for InputPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Each declaration is one conjunct. Length counts Unicode scalar values,
/// matching ordinary String refinements. Empty strings satisfy AllowedChars
/// and NoAdjacent, but fail FirstChar and LastChar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPredicate<P> {
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
/// ```compile_fail
/// use carina_core::schema::{Compiled, InputRule};
/// fn bypass(rule: &InputRule<Compiled>, observed: &str) {
///     let _ = rule.evaluate(observed);
/// }
/// ```
/// ```compile_fail
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
    predicate: InputPredicate<R::Pattern>,
}

/// Exhaustive read-only declaration view for transport and fingerprinting.
#[derive(Debug, Clone, Copy)]
pub struct InputRuleParts<'a, R: PatternForm = Draft> {
    pub scope: InputRuleScope,
    pub target: &'a InputRuleTarget,
    pub code: &'a InputRuleCode,
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
            predicate: &self.predicate,
        }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InputViolationReason {
    #[error("length is outside the permitted character-count bounds")]
    Length,
    #[error("contains a character outside the permitted set")]
    AllowedChars,
    #[error("first character is outside the permitted set or the string is empty")]
    FirstChar,
    #[error("last character is outside the permitted set or the string is empty")]
    LastChar,
    #[error("contains a forbidden substring")]
    NoSubstring,
    #[error("has a forbidden prefix")]
    NoPrefix,
    #[error("has a forbidden prefix")]
    ForbiddenPrefix,
    #[error("has a forbidden suffix")]
    NoSuffix,
    #[error("contains a forbidden adjacent character pair")]
    NoAdjacent,
    #[error("has IPv4 dotted-quad form")]
    NotIpv4,
    #[error("does not match the required pattern")]
    Matches,
    #[error("matches a forbidden pattern")]
    NotMatches,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path} [{code}]: {reason}")]
pub struct InputViolation {
    pub(crate) path: InputPath,
    pub(crate) code: InputRuleCode,
    pub(crate) reason: InputViolationReason,
}

impl InputViolation {
    pub fn path(&self) -> &InputPath {
        &self.path
    }
    pub fn code(&self) -> &InputRuleCode {
        &self.code
    }
    pub fn reason(&self) -> InputViolationReason {
        self.reason
    }
}

impl InputRule<Draft> {
    pub(super) fn compile_patterns(
        self,
        index: usize,
        issues: &mut Vec<SchemaDefinitionIssue>,
    ) -> Option<InputRule<Compiled>> {
        let Self {
            scope,
            target,
            code,
            predicate,
        } = self;
        let mut compile = |source: String| match CompiledPattern::try_new(&source) {
            Ok(pattern) => Some(pattern),
            Err(error) => {
                let location = SchemaValidationIssueLocation::InputRule(index);
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
        Some(InputRule::new(scope, target, code, predicate))
    }
}

impl<R: PatternForm> PatternResourceSchema<R> {
    pub(super) fn validate_input_rules(&self, issues: &mut Vec<SchemaDefinitionIssue>) {
        let mut seen = HashSet::new();
        for (index, rule) in self.input_rules.iter().enumerate() {
            let mut reject = |error: InputRuleError| {
                let location = SchemaValidationIssueLocation::InputRule(index);
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
                    (Some(min), Some(max)) if min > max => {
                        reject(InputRuleError::InvertedLengthBounds)
                    }
                    (Some(_), Some(_)) | (None, Some(_)) | (Some(_), None) => {}
                }
            }
            if let Err(error) = self.input_target_type(rule.target()) {
                reject(error);
            }
        }
    }

    fn input_target_type(&self, target: &InputRuleTarget) -> Result<(), InputRuleError> {
        let path = target.path();
        let mut segments = path.segments();
        let root = segments.next().expect("input paths are nonempty");
        let attribute = self
            .attributes
            .get(root)
            .ok_or(InputRuleError::UnknownPath)?;
        let mut ty = &attribute.attr_type;
        let mut provider_populated = attribute.input_mode.is_provider_populated();
        for segment in segments {
            ty = peel_refs(ty, &self.defs).ok_or(InputRuleError::UnknownPath)?;
            let AttrTypeKind::Struct { fields, .. } = &ty.kind else {
                return Err(InputRuleError::UnknownPath);
            };
            let field = fields
                .iter()
                .find(|field| field.name == segment)
                .ok_or(InputRuleError::UnknownPath)?;
            provider_populated |= field.declared_input_mode.is_provider_populated();
            ty = &field.field_type;
        }
        if provider_populated {
            return Err(InputRuleError::ProviderPopulatedPath);
        }
        match target {
            InputRuleTarget::Value(_) => {
                if string_based(ty, &self.defs, &mut HashSet::new()) {
                    Ok(())
                } else {
                    Err(InputRuleError::NonStringPath)
                }
            }
            InputRuleTarget::MapKeys(_) => {
                let ty = peel_refs(ty, &self.defs).ok_or(InputRuleError::UnknownPath)?;
                match &ty.kind {
                    AttrTypeKind::Map { key, .. }
                        if string_based(key, &self.defs, &mut HashSet::new()) =>
                    {
                        Ok(())
                    }
                    _ => Err(InputRuleError::NonStringKeyMapPath),
                }
            }
        }
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
