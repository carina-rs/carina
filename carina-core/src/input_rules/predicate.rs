//! Internal evaluation views of the provider-facing predicate declaration.

use crate::schema::{CharSet, InputLiteral, InputPredicate, NonEmptyString};

pub(crate) enum PredicateClass<'a, P> {
    Presence(Presence),
    Equals(&'a InputLiteral),
    Secret,
    String(StringPredicate<'a, P>),
}

pub(crate) enum Presence {
    Present,
    Absent,
}

/// Only the classifier can construct access to plaintext string evaluation.
pub(crate) struct StringPredicate<'a, P> {
    kind: StringPredicateKind<'a, P>,
}

/// The closed set of string operations, borrowing declaration data.
pub(crate) enum StringPredicateKind<'a, P> {
    Length {
        min: Option<u64>,
        max: Option<u64>,
    },
    AllowedChars(&'a CharSet),
    FirstChar(&'a CharSet),
    LastChar(&'a CharSet),
    NoSubstring(&'a NonEmptyString),
    NoPrefix(&'a NonEmptyString),
    ForbiddenPrefix {
        prefix: &'a NonEmptyString,
        ignore_ascii_case: bool,
    },
    NoSuffix(&'a NonEmptyString),
    NoAdjacent {
        left: &'a CharSet,
        right: &'a CharSet,
    },
    NotIpv4,
    Matches(&'a P),
    NotMatches(&'a P),
}

impl<'a, P> StringPredicate<'a, P> {
    pub(crate) fn kind(&self) -> &StringPredicateKind<'a, P> {
        &self.kind
    }
}

/// This is the sole bridge from declarations to evaluation classes. Every new
/// public predicate must explicitly choose whether it can consume plaintext.
pub(crate) fn classify<P>(predicate: &InputPredicate<P>) -> PredicateClass<'_, P> {
    let kind = match predicate {
        InputPredicate::Present => return PredicateClass::Presence(Presence::Present),
        InputPredicate::Absent => return PredicateClass::Presence(Presence::Absent),
        InputPredicate::Equals(literal) => return PredicateClass::Equals(literal),
        InputPredicate::Secret => return PredicateClass::Secret,
        InputPredicate::Length { min, max } => StringPredicateKind::Length {
            min: *min,
            max: *max,
        },
        InputPredicate::AllowedChars(set) => StringPredicateKind::AllowedChars(set),
        InputPredicate::FirstChar(set) => StringPredicateKind::FirstChar(set),
        InputPredicate::LastChar(set) => StringPredicateKind::LastChar(set),
        InputPredicate::NoSubstring(token) => StringPredicateKind::NoSubstring(token),
        InputPredicate::NoPrefix(token) => StringPredicateKind::NoPrefix(token),
        InputPredicate::ForbiddenPrefix {
            prefix,
            ignore_ascii_case,
        } => StringPredicateKind::ForbiddenPrefix {
            prefix,
            ignore_ascii_case: *ignore_ascii_case,
        },
        InputPredicate::NoSuffix(token) => StringPredicateKind::NoSuffix(token),
        InputPredicate::NoAdjacent { left, right } => {
            StringPredicateKind::NoAdjacent { left, right }
        }
        InputPredicate::NotIpv4 => StringPredicateKind::NotIpv4,
        InputPredicate::Matches(pattern) => StringPredicateKind::Matches(pattern),
        InputPredicate::NotMatches(pattern) => StringPredicateKind::NotMatches(pattern),
    };
    PredicateClass::String(StringPredicate { kind })
}
