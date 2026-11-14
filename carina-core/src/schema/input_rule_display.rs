//! One declaration spelling shared by provider documentation and input findings.

use std::fmt;

use crate::provider_text::{DiagnosticQuote, DiagnosticText, DiagnosticTextBuilder};

use super::{
    CharSet, Draft, InputCondition, InputPredicate, InputRule, InputRuleScope, InputRuleTarget,
    PatternForm, PatternSpec, PatternSpecDisplay,
};

/// A rule's declaration, including its target, condition and write scope.
///
/// Paths are relative to the declaring resource or Struct. Only provider-declared
/// parameters are rendered; this adapter cannot receive a rejected input value.
/// Text uses the diagnostic sanitizer, without Markdown escaping. Draft and
/// admitted rules have the same spelling, including complete pattern metadata.
#[derive(Clone, Copy)]
pub struct InputRuleDisplay<'a, R: PatternForm = Draft> {
    rule: &'a InputRule<R>,
}

impl<'a, R: PatternForm> InputRuleDisplay<'a, R> {
    pub fn new(rule: &'a InputRule<R>) -> Self {
        Self { rule }
    }

    pub(super) fn requirement(self) -> impl fmt::Display + 'a {
        Requirement(self.rule)
    }
}

impl<R: PatternForm> fmt::Display for InputRuleDisplay<'_, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.rule.target() {
            InputRuleTarget::Value(path) => {
                write!(f, "value at {path}: ")?;
            }
            InputRuleTarget::MapKeys(path) => {
                write!(f, "map keys at {path}: ")?;
            }
        }
        self.requirement().fmt(f)?;
        match self.rule.scope() {
            InputRuleScope::CreateOnly => f.write_str(" (on create)"),
            InputRuleScope::EveryWrite => f.write_str(" (on every write)"),
        }
    }
}

// The requirement has its own budget so its text, including any truncation
// marker, is identical inside a declaration and a violation message.
struct Requirement<'a, R: PatternForm>(&'a InputRule<R>);

impl<R: PatternForm> fmt::Display for Requirement<'_, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = DiagnosticTextBuilder::default();
        push_predicate(&mut text, self.0.predicate());
        push_condition_suffix(&mut text, self.0.condition());
        text.finish().fmt(f)
    }
}

pub(super) fn evaluation_failure(condition: &InputCondition) -> DiagnosticText {
    let mut text = DiagnosticTextBuilder::default();
    text.push_str("input evaluation failed");
    push_condition_suffix(&mut text, condition);
    text.finish()
}

fn push_condition_suffix(text: &mut DiagnosticTextBuilder, condition: &InputCondition) {
    match condition {
        InputCondition::Always => {}
        condition @ (InputCondition::Present(_)
        | InputCondition::Equals {
            path: _,
            literal: _,
        }
        | InputCondition::Not(_)) => {
            text.push_str(" when ");
            condition.push_to(text);
        }
    }
}

fn push_predicate<P: AsRef<PatternSpec>>(
    text: &mut DiagnosticTextBuilder,
    predicate: &InputPredicate<P>,
) {
    match predicate {
        InputPredicate::Present => text.push_str("must be present"),
        InputPredicate::Absent => text.push_str("must be absent"),
        InputPredicate::Secret => {
            text.push_str("must be a secret value (for example `secret(...)` or `decrypt(...)`)")
        }
        InputPredicate::Equals(literal) => {
            text.push_str("must equal ");
            literal.push_to(text);
        }
        InputPredicate::Length { min, max } => {
            text.push_str("must have length ");
            if let Some(min) = min {
                text.push_str(&min.to_string());
            }
            text.push_str("..");
            if let Some(max) = max {
                text.push_str("=");
                text.push_str(&max.to_string());
            }
            text.push_str(" Unicode scalar values");
        }
        InputPredicate::AllowedChars(set) => {
            text.push_str("must contain only characters in ");
            push_char_set(text, set);
        }
        InputPredicate::FirstChar(set) => {
            text.push_str("must start with a character in ");
            push_char_set(text, set);
        }
        InputPredicate::LastChar(set) => {
            text.push_str("must end with a character in ");
            push_char_set(text, set);
        }
        InputPredicate::NoSubstring(token) => {
            text.push_str("must not contain ");
            text.push_quoted_str(token.as_str(), DiagnosticQuote::Double);
        }
        InputPredicate::NoPrefix(token) => {
            text.push_str("must not start with ");
            text.push_quoted_str(token.as_str(), DiagnosticQuote::Double);
        }
        InputPredicate::ForbiddenPrefix {
            prefix,
            ignore_ascii_case,
        } => {
            text.push_str("must not start with ");
            text.push_quoted_str(prefix.as_str(), DiagnosticQuote::Double);
            if *ignore_ascii_case {
                text.push_str(" (ignoring ASCII case)");
            }
        }
        InputPredicate::NoSuffix(token) => {
            text.push_str("must not end with ");
            text.push_quoted_str(token.as_str(), DiagnosticQuote::Double);
        }
        InputPredicate::NoAdjacent { left, right } => {
            text.push_str("must not contain an adjacent pair from ");
            push_char_set(text, left);
            text.push_str(" then ");
            push_char_set(text, right);
        }
        InputPredicate::NotIpv4 => text.push_str("must not have IPv4 dotted-quad form"),
        InputPredicate::Matches(pattern) => {
            text.push_str("must match ");
            PatternSpecDisplay::new(pattern.as_ref()).push_to(text);
        }
        InputPredicate::NotMatches(pattern) => {
            text.push_str("must not match ");
            PatternSpecDisplay::new(pattern.as_ref()).push_to(text);
        }
    }
}

fn push_char_set(text: &mut DiagnosticTextBuilder, set: &CharSet) {
    text.push_str("{");
    let mut utf8 = [0; 4];
    for (index, (start, end)) in set.ranges().iter().enumerate() {
        if index != 0 {
            text.push_str(", ");
        }
        text.push_quoted_str(start.encode_utf8(&mut utf8), DiagnosticQuote::Single);
        if start != end {
            text.push_str("..=");
            text.push_quoted_str(end.encode_utf8(&mut utf8), DiagnosticQuote::Single);
        }
    }
    text.push_str("}");
}
