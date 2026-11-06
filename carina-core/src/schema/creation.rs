//! Portable, creation-only string declarations. Admission checks their shape;
//! ordinary value validation does not evaluate them. An absent optional input
//! is exempt, while unresolved presence must be handled by creation admission.

use std::{collections::HashSet, fmt};

use super::{
    AttrTypeKind, Compiled, CompiledPattern, Draft, PatternAttributeType, PatternForm,
    PatternResourceSchema, SchemaDefinitionIssue, SchemaValidationIssueLocation,
};

/// A malformed creation declaration, independent of any input value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreationConstraintError {
    #[error("creation input path must be a dotted sequence of attribute identifiers")]
    InvalidPath,
    #[error("creation rule code must contain lowercase identifier segments separated by dots")]
    InvalidCode,
    #[error("forbidden creation token must not be empty")]
    EmptyToken,
    #[error("creation character set must not be empty")]
    EmptyCharSet,
    #[error("creation character range has inverted endpoints")]
    InvertedCharRange,
    #[error("creation character range endpoint is not a Unicode scalar")]
    InvalidUnicodeScalar,
    #[error("creation length must have at least one bound")]
    EmptyLengthBounds,
    #[error("creation length minimum exceeds maximum")]
    InvertedLengthBounds,
    #[error("creation input path does not name a canonical attribute or struct field")]
    UnknownPath,
    #[error("creation input path is provider-populated")]
    ProviderPopulatedPath,
    #[error("creation input path must have a String-based type")]
    NonStringPath,
    #[error("duplicate creation rule path and code")]
    DuplicateRule,
}

/// A nonempty union of inclusive Unicode scalar ranges, sorted and coalesced.
/// Surrogates are unrepresentable because endpoints are Rust `char`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharSet(Vec<(char, char)>);

impl CharSet {
    pub fn new(
        ranges: impl IntoIterator<Item = (char, char)>,
    ) -> Result<Self, CreationConstraintError> {
        let mut ranges: Vec<_> = ranges.into_iter().collect();
        if ranges.is_empty() {
            return Err(CreationConstraintError::EmptyCharSet);
        }
        if ranges.iter().any(|(start, end)| start > end) {
            return Err(CreationConstraintError::InvertedCharRange);
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
    pub fn new(value: impl Into<String>) -> Result<Self, CreationConstraintError> {
        let value = value.into();
        if value.is_empty() {
            Err(CreationConstraintError::EmptyToken)
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
pub struct CreationRuleCode(String);

impl CreationRuleCode {
    pub fn new(value: impl Into<String>) -> Result<Self, CreationConstraintError> {
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
            Err(CreationConstraintError::InvalidCode)
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CreationRuleCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A canonical dotted attribute/struct-field path, without block aliases or
/// collection wildcards. Schema admission proves it names a writable string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CreationInputPath(String);

impl CreationInputPath {
    pub fn new(value: impl Into<String>) -> Result<Self, CreationConstraintError> {
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
            Err(CreationConstraintError::InvalidPath)
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn segments(&self) -> std::str::Split<'_, char> {
        self.0.split('.')
    }
}

impl fmt::Display for CreationInputPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Each declaration is one conjunct. Length counts Unicode scalar values,
/// matching ordinary String refinements. Empty strings satisfy AllowedChars
/// and NoAdjacent, but fail FirstChar and LastChar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreationPredicate<P> {
    Length { min: Option<u64>, max: Option<u64> },
    AllowedChars(CharSet),
    FirstChar(CharSet),
    LastChar(CharSet),
    NoSubstring(NonEmptyString),
    NoPrefix(NonEmptyString),
    NoSuffix(NonEmptyString),
    NoAdjacent { left: CharSet, right: CharSet },
    NotIpv4,
    Matches(P),
    NotMatches(P),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreationRule<R: PatternForm = Draft> {
    path: CreationInputPath,
    code: CreationRuleCode,
    predicate: CreationPredicate<R::Pattern>,
}

/// Exhaustive read-only declaration view for transport and fingerprinting.
#[derive(Debug, Clone, Copy)]
pub struct CreationRuleParts<'a, R: PatternForm = Draft> {
    pub path: &'a CreationInputPath,
    pub code: &'a CreationRuleCode,
    pub predicate: &'a CreationPredicate<R::Pattern>,
}

impl<R: PatternForm> CreationRule<R> {
    pub fn new(
        path: CreationInputPath,
        code: CreationRuleCode,
        predicate: CreationPredicate<R::Pattern>,
    ) -> Self {
        Self {
            path,
            code,
            predicate,
        }
    }
    pub fn parts(&self) -> CreationRuleParts<'_, R> {
        CreationRuleParts {
            path: &self.path,
            code: &self.code,
            predicate: &self.predicate,
        }
    }
    pub fn path(&self) -> &CreationInputPath {
        &self.path
    }
    pub fn code(&self) -> &CreationRuleCode {
        &self.code
    }
    pub fn predicate(&self) -> &CreationPredicate<R::Pattern> {
        &self.predicate
    }
}

/// A safe reason that never retains the rejected input string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreationViolationReason {
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
pub struct CreationViolation {
    path: CreationInputPath,
    code: CreationRuleCode,
    reason: CreationViolationReason,
}

impl CreationViolation {
    pub fn path(&self) -> &CreationInputPath {
        &self.path
    }
    pub fn code(&self) -> &CreationRuleCode {
        &self.code
    }
    pub fn reason(&self) -> CreationViolationReason {
        self.reason
    }
}

impl CreationRule<Compiled> {
    /// Evaluate one known, present string. This pure helper is not evidence
    /// that an effect is eligible for creation or that a plan was checked.
    pub fn evaluate(&self, value: &str) -> Result<(), CreationViolation> {
        let (accepted, reason) = match &self.predicate {
            CreationPredicate::Length { min, max } => {
                let count = u64::try_from(value.chars().count()).expect("string length fits u64");
                (
                    min.is_none_or(|min| count >= min) && max.is_none_or(|max| count <= max),
                    CreationViolationReason::Length,
                )
            }
            CreationPredicate::AllowedChars(set) => (
                value.chars().all(|ch| set.contains(ch)),
                CreationViolationReason::AllowedChars,
            ),
            CreationPredicate::FirstChar(set) => (
                value.chars().next().is_some_and(|ch| set.contains(ch)),
                CreationViolationReason::FirstChar,
            ),
            CreationPredicate::LastChar(set) => (
                value.chars().next_back().is_some_and(|ch| set.contains(ch)),
                CreationViolationReason::LastChar,
            ),
            CreationPredicate::NoSubstring(token) => (
                !value.contains(token.as_str()),
                CreationViolationReason::NoSubstring,
            ),
            CreationPredicate::NoPrefix(token) => (
                !value.starts_with(token.as_str()),
                CreationViolationReason::NoPrefix,
            ),
            CreationPredicate::NoSuffix(token) => (
                !value.ends_with(token.as_str()),
                CreationViolationReason::NoSuffix,
            ),
            CreationPredicate::NoAdjacent { left, right } => (
                !value
                    .chars()
                    .zip(value.chars().skip(1))
                    .any(|(a, b)| left.contains(a) && right.contains(b)),
                CreationViolationReason::NoAdjacent,
            ),
            CreationPredicate::NotIpv4 => (!is_ipv4(value), CreationViolationReason::NotIpv4),
            CreationPredicate::Matches(pattern) => {
                (pattern.is_match(value), CreationViolationReason::Matches)
            }
            CreationPredicate::NotMatches(pattern) => (
                !pattern.is_match(value),
                CreationViolationReason::NotMatches,
            ),
        };
        if accepted {
            Ok(())
        } else {
            Err(CreationViolation {
                path: self.path.clone(),
                code: self.code.clone(),
                reason,
            })
        }
    }
}

/// Strict decimal dotted quad: exactly four ASCII octets in 0..=255, without
/// signs, whitespace or leading zeroes on multi-digit octets.
fn is_ipv4(value: &str) -> bool {
    let mut octets = value.split('.');
    for _ in 0..4 {
        let Some(octet) = octets.next() else {
            return false;
        };
        if octet.is_empty()
            || octet.len() > 3
            || (octet.len() > 1 && octet.starts_with('0'))
            || !octet.bytes().all(|byte| byte.is_ascii_digit())
            || octet.parse::<u8>().is_err()
        {
            return false;
        }
    }
    octets.next().is_none()
}

impl CreationRule<Draft> {
    pub(super) fn compile_patterns(
        self,
        index: usize,
        issues: &mut Vec<SchemaDefinitionIssue>,
    ) -> Option<CreationRule<Compiled>> {
        let Self {
            path,
            code,
            predicate,
        } = self;
        let mut compile = |source: String| match CompiledPattern::try_new(&source) {
            Ok(pattern) => Some(pattern),
            Err(error) => {
                let location = SchemaValidationIssueLocation::CreationConstraint(index);
                issues.push(SchemaDefinitionIssue {
                    path: location.raw_path().into(),
                    location,
                    message: error.to_string().into(),
                    pattern_error: Some(error),
                    creation_error: None,
                });
                None
            }
        };
        let predicate = match predicate {
            CreationPredicate::Length { min, max } => CreationPredicate::Length { min, max },
            CreationPredicate::AllowedChars(set) => CreationPredicate::AllowedChars(set),
            CreationPredicate::FirstChar(set) => CreationPredicate::FirstChar(set),
            CreationPredicate::LastChar(set) => CreationPredicate::LastChar(set),
            CreationPredicate::NoSubstring(token) => CreationPredicate::NoSubstring(token),
            CreationPredicate::NoPrefix(token) => CreationPredicate::NoPrefix(token),
            CreationPredicate::NoSuffix(token) => CreationPredicate::NoSuffix(token),
            CreationPredicate::NoAdjacent { left, right } => {
                CreationPredicate::NoAdjacent { left, right }
            }
            CreationPredicate::NotIpv4 => CreationPredicate::NotIpv4,
            CreationPredicate::Matches(pattern) => CreationPredicate::Matches(compile(pattern)?),
            CreationPredicate::NotMatches(pattern) => {
                CreationPredicate::NotMatches(compile(pattern)?)
            }
        };
        Some(CreationRule::new(path, code, predicate))
    }
}

impl<R: PatternForm> PatternResourceSchema<R> {
    pub(super) fn validate_creation_constraints(&self, issues: &mut Vec<SchemaDefinitionIssue>) {
        let mut seen = HashSet::new();
        for (index, rule) in self.creation_constraints.iter().enumerate() {
            let mut reject = |error: CreationConstraintError| {
                let location = SchemaValidationIssueLocation::CreationConstraint(index);
                issues.push(SchemaDefinitionIssue {
                    path: location.raw_path().into(),
                    location,
                    message: error.to_string().into(),
                    pattern_error: None,
                    creation_error: Some(error),
                });
            };
            if !seen.insert((rule.path(), rule.code())) {
                reject(CreationConstraintError::DuplicateRule);
            }
            if let CreationPredicate::Length { min, max } = rule.predicate() {
                match (min, max) {
                    (None, None) => reject(CreationConstraintError::EmptyLengthBounds),
                    (Some(min), Some(max)) if min > max => {
                        reject(CreationConstraintError::InvertedLengthBounds)
                    }
                    (Some(_), Some(_)) | (None, Some(_)) | (Some(_), None) => {}
                }
            }
            if let Err(error) = self.creation_path_type(rule.path()) {
                reject(error);
            }
        }
    }

    fn creation_path_type(&self, path: &CreationInputPath) -> Result<(), CreationConstraintError> {
        let mut segments = path.segments();
        let root = segments.next().expect("creation paths are nonempty");
        let attribute = self
            .attributes
            .get(root)
            .ok_or(CreationConstraintError::UnknownPath)?;
        let mut ty = &attribute.attr_type;
        let mut provider_populated = attribute.input_mode.is_provider_populated();
        for segment in segments {
            ty = peel_refs(ty, &self.defs).ok_or(CreationConstraintError::UnknownPath)?;
            let AttrTypeKind::Struct { fields, .. } = &ty.kind else {
                return Err(CreationConstraintError::UnknownPath);
            };
            let field = fields
                .iter()
                .find(|field| field.name == segment)
                .ok_or(CreationConstraintError::UnknownPath)?;
            provider_populated |= field.declared_input_mode.is_provider_populated();
            ty = &field.field_type;
        }
        if provider_populated {
            return Err(CreationConstraintError::ProviderPopulatedPath);
        }
        if string_based(ty, &self.defs, &mut HashSet::new()) {
            Ok(())
        } else {
            Err(CreationConstraintError::NonStringPath)
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
