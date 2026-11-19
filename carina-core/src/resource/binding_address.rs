use std::fmt;
use std::hash::Hash;

use serde::{Deserialize, Deserializer, Serialize};

use super::ValueString;

/// A checked decomposition of an existing binding spelling. This grants no
/// authority to construct a new resource address.
#[derive(Debug)]
pub struct CheckedBindingAddress<'a> {
    spelling: &'a str,
    ends: Vec<usize>,
}

impl<'a> CheckedBindingAddress<'a> {
    pub fn candidate_ancestors(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.ends[..self.ends.len() - 1]
            .iter()
            .map(|&end| &self.spelling[..end])
    }

    pub fn is_direct_child_of(&self, parent: &str) -> bool {
        self.ends.len() > 1 && &self.spelling[..self.ends[self.ends.len() - 2]] == parent
    }

    pub fn is_descendant_of(&self, ancestor: &str) -> bool {
        self.candidate_ancestors()
            .any(|candidate| candidate == ancestor)
    }

    pub fn is_self_or_descendant_of(&self, ancestor: &str) -> bool {
        self.spelling == ancestor || self.is_descendant_of(ancestor)
    }
}

/// Decompose dot fields, numeric subscripts, and existing quoted-key spelling.
/// Key contents are opaque: this neither escapes nor normalizes them.
pub fn decompose_binding_address(spelling: &str) -> Option<CheckedBindingAddress<'_>> {
    let bytes = spelling.as_bytes();
    let root_end = bytes
        .iter()
        .position(|b| matches!(b, b'.' | b'['))
        .unwrap_or(bytes.len());
    if root_end == 0 {
        return None;
    }
    // A closing quote may itself be key content. Keep only closures whose
    // remaining suffix is a valid address, without interpreting key escapes.
    let mut next = vec![None; bytes.len() + 1];
    next[bytes.len()] = Some(bytes.len());
    let mut single_closures = Vec::new();
    let mut double_closures = Vec::new();
    for cursor in (root_end..bytes.len()).rev() {
        next[cursor] = match bytes[cursor] {
            b'.' => {
                let start = cursor + 1;
                let mut end = start;
                while end < bytes.len() && !matches!(bytes[end], b'.' | b'[') {
                    end += 1;
                }
                (crate::utils::is_identifier_safe(&spelling[start..end]) && next[end].is_some())
                    .then_some(end)
            }
            b'[' => match bytes.get(cursor + 1).copied() {
                Some(quote @ (b'\'' | b'"')) => {
                    let closures = if quote == b'\'' {
                        &single_closures
                    } else {
                        &double_closures
                    };
                    closures
                        .iter()
                        .rev()
                        .find(|&&closing| closing >= cursor + 2)
                        .map(|closing| closing + 2)
                }
                Some(b'0'..=b'9') => {
                    let mut end = cursor + 1;
                    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                        end += 1;
                    }
                    (bytes.get(end) == Some(&b']') && next[end + 1].is_some()).then_some(end + 1)
                }
                _ => None,
            },
            _ => None,
        };
        if matches!(bytes[cursor], b'\'' | b'"')
            && bytes.get(cursor + 1) == Some(&b']')
            && next[cursor + 2].is_some()
        {
            if bytes[cursor] == b'\'' {
                single_closures.push(cursor);
            } else {
                double_closures.push(cursor);
            }
        }
    }
    let mut ends = vec![root_end];
    let mut cursor = root_end;
    while cursor < bytes.len() {
        cursor = next[cursor]?;
        ends.push(cursor);
    }
    Some(CheckedBindingAddress { spelling, ends })
}

mod private {
    use super::ValueString;

    pub struct Derivation;

    pub trait Sealed: Sized {
        fn deferred_template(&self, authority: Derivation) -> Self;
        fn selected_attribute(&self, attribute: &str, authority: Derivation) -> Self;
        fn collection_binding(&self, index: usize, authority: Derivation) -> Self;

        fn with_index(&self, index: i64, authority: Derivation) -> Self;

        fn with_map_key(&self, key: &ValueString, authority: Derivation) -> Self;

        fn module_argument_placeholder(name: String, authority: Derivation) -> Option<Self>;

        fn binding_in_scope(
            name: super::RelativeBindingName,
            scope: Option<&str>,
            authority: Derivation,
        ) -> Self;

        fn knowledge_diagnostic(
            error: super::super::knowledge::KnowledgeError<Self>,
        ) -> super::super::knowledge::KnowledgeDiagnostic
        where
            Self: super::BindingAddress;

        fn input_preview<P: super::super::ValueKind>(
            value: super::super::Value<P, Self>,
        ) -> super::super::Value<P>
        where
            Self: super::BindingAddress;
    }
}

/// A binding name exactly as authored inside one root or module directory.
///
/// Relative names are parser input. They cannot be compared with
/// [`AbsoluteBindingAddress`]; crossing a scope boundary requires one of the
/// typed scope resolvers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelativeBindingName(ValueString);

impl RelativeBindingName {
    pub fn new(name: impl Into<ValueString>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        self.0.expose_plaintext()
    }

    pub fn into_string(self) -> String {
        self.0.into_plaintext()
    }

    pub(super) fn value_string(&self) -> &ValueString {
        &self.0
    }

    pub(super) fn into_value_string(self) -> ValueString {
        self.0
    }
}

impl From<String> for RelativeBindingName {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<&str> for RelativeBindingName {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

impl From<ValueString> for RelativeBindingName {
    fn from(value: ValueString) -> Self {
        Self(value)
    }
}

impl std::ops::Deref for RelativeBindingName {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for RelativeBindingName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for RelativeBindingName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for RelativeBindingName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A binding address resolved into the scope used by planning and execution.
///
/// The field is private and this type intentionally implements neither
/// `From<String>` nor `Default`. Live code can obtain one only from a typed
/// scope resolver. Deserialization uses a separate persisted-address path and
/// preserves the historical transparent string wire format.
///
/// An absolute address cannot be constructed from an unscoped string:
///
/// ```compile_fail
/// use carina_core::resource::AbsoluteBindingAddress;
///
/// let _: AbsoluteBindingAddress = String::from("service").into();
/// ```
///
/// Relative and absolute names belong to distinct phases and cannot be
/// compared directly:
///
/// ```compile_fail
/// use carina_core::resource::{AbsoluteBindingAddress, RelativeBindingName};
///
/// # fn compare(relative: RelativeBindingName, absolute: AbsoluteBindingAddress) {
/// let _ = relative == absolute;
/// # }
/// ```
///
/// An absolute address cannot be used as a string-borrowing map key. Lookups
/// must use another absolute address, so a relative or unparsed spelling
/// cannot silently cross the phase boundary:
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::resource::AbsoluteBindingAddress;
///
/// # fn lookup(map: &HashMap<AbsoluteBindingAddress, usize>) {
/// let _ = map.get("service");
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct AbsoluteBindingAddress(ValueString);

impl AbsoluteBindingAddress {
    pub fn as_str(&self) -> &str {
        self.0.expose_plaintext()
    }

    pub fn into_string(self) -> String {
        self.0.into_plaintext()
    }

    pub(super) fn from_scope(value: ValueString) -> Self {
        Self(value)
    }

    /// Derive a selected module output while retaining its resolved scope.
    pub(crate) fn with_attribute(&self, attribute: &str) -> Self {
        Self(ValueString::derived_from(
            format!("{}.{attribute}", self.as_str()),
            [&self.0],
        ))
    }

    pub(crate) fn with_index(&self, index: i64) -> Self {
        Self(ValueString::derived_from(
            format!("{}[{index}]", self.as_str()),
            [&self.0],
        ))
    }

    pub(crate) fn with_map_key(&self, key: &ValueString) -> Self {
        Self(ValueString::derived_from(
            crate::utils::map_key_address(self.as_str(), key.expose_plaintext()),
            [&self.0, key],
        ))
    }

    pub(crate) fn remap_instance_prefix(&self, from: &str, to: &str) -> Option<Self> {
        let suffix = self.as_str().strip_prefix(from)?;
        if !suffix.is_empty() && !suffix.starts_with('.') {
            return None;
        }
        Some(Self(ValueString::derived_from(
            format!("{to}{suffix}"),
            [&self.0],
        )))
    }
}

const EMPTY_ABSOLUTE_BINDING_ADDRESS: &str = "an absolute binding address cannot be empty";

fn validate_absolute_binding_address(value: ValueString) -> Result<ValueString, &'static str> {
    if value.expose_plaintext().is_empty() {
        Err(EMPTY_ABSOLUTE_BINDING_ADDRESS)
    } else {
        Ok(value)
    }
}

/// Construct an absolute address for a fixture that cannot practically be
/// built through parse-and-expand.
#[cfg(any(test, feature = "test-utils"))]
pub fn absolute_binding_address_for_test(spelling: impl Into<String>) -> AbsoluteBindingAddress {
    AbsoluteBindingAddress(spelling.into().into())
}

impl<'de> Deserialize<'de> for AbsoluteBindingAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ValueString::deserialize(deserializer)?;
        validate_absolute_binding_address(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl AsRef<str> for AbsoluteBindingAddress {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for AbsoluteBindingAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Shared read-only behavior for phase-specific binding names.
///
/// The trait is sealed so downstream code cannot introduce a third phase that
/// bypasses scope resolution.
pub trait BindingAddress:
    private::Sealed + Clone + fmt::Debug + fmt::Display + Eq + Hash + Ord + Send + Sync + 'static
{
    /// Address derivation is intentionally unavailable through this public
    /// trait. In particular, downstream code cannot append a persisted map
    /// key to an absolute address:
    ///
    /// ```compile_fail
    /// use carina_core::resource::{AbsoluteBindingAddress, BindingAddress, ValueString};
    ///
    /// # fn derive(address: &AbsoluteBindingAddress, key: &ValueString) {
    /// let _ = <AbsoluteBindingAddress as BindingAddress>::with_map_key(address, key);
    /// # }
    /// ```
    fn as_str(&self) -> &str;
}

impl private::Sealed for RelativeBindingName {
    fn input_preview<P: super::ValueKind>(value: super::Value<P, Self>) -> super::Value<P> {
        // Parser-owned directories have root-relative bindings. Qualification
        // preserves their structure; preview knowledge never resolves them.
        let scope = super::RootScope::new();
        value.resolve_bindings(&mut |binding| scope.resolve(binding))
    }

    fn knowledge_diagnostic(
        error: super::knowledge::KnowledgeError<Self>,
    ) -> super::knowledge::KnowledgeDiagnostic {
        super::knowledge::KnowledgeDiagnostic::Relative(error)
    }
    fn selected_attribute(&self, attribute: &str, _authority: private::Derivation) -> Self {
        Self(ValueString::derived_from(
            format!("{}.{attribute}", self.as_str()),
            [&self.0],
        ))
    }
    fn collection_binding(&self, index: usize, _authority: private::Derivation) -> Self {
        Self(ValueString::derived_from(
            self.as_str()[..index].to_owned(),
            [&self.0],
        ))
    }

    fn deferred_template(&self, _authority: private::Derivation) -> Self {
        Self(ValueString::derived_from(
            format!("{}[?]", self.as_str()),
            [&self.0],
        ))
    }

    fn with_index(&self, index: i64, _authority: private::Derivation) -> Self {
        Self(ValueString::derived_from(
            format!("{}[{index}]", self.as_str()),
            [&self.0],
        ))
    }

    fn with_map_key(&self, key: &ValueString, _authority: private::Derivation) -> Self {
        Self(ValueString::derived_from(
            crate::utils::map_key_address(self.as_str(), key.expose_plaintext()),
            [&self.0, key],
        ))
    }

    fn module_argument_placeholder(name: String, _authority: private::Derivation) -> Option<Self> {
        Some(Self::new(name))
    }

    fn binding_in_scope(
        name: RelativeBindingName,
        _scope: Option<&str>,
        _authority: private::Derivation,
    ) -> Self {
        name
    }
}

impl BindingAddress for RelativeBindingName {
    fn as_str(&self) -> &str {
        self.as_str()
    }
}

impl private::Sealed for AbsoluteBindingAddress {
    fn input_preview<P: super::ValueKind>(value: super::Value<P, Self>) -> super::Value<P> {
        value
    }

    fn knowledge_diagnostic(
        error: super::knowledge::KnowledgeError<Self>,
    ) -> super::knowledge::KnowledgeDiagnostic {
        super::knowledge::KnowledgeDiagnostic::Absolute(error)
    }
    fn selected_attribute(&self, attribute: &str, _authority: private::Derivation) -> Self {
        Self(ValueString::derived_from(
            format!("{}.{attribute}", self.as_str()),
            [&self.0],
        ))
    }
    fn collection_binding(&self, index: usize, _authority: private::Derivation) -> Self {
        Self(ValueString::derived_from(
            self.as_str()[..index].to_owned(),
            [&self.0],
        ))
    }

    fn deferred_template(&self, _authority: private::Derivation) -> Self {
        Self(ValueString::derived_from(
            format!("{}[?]", self.as_str()),
            [&self.0],
        ))
    }

    fn with_index(&self, index: i64, _authority: private::Derivation) -> Self {
        Self::with_index(self, index)
    }

    fn with_map_key(&self, key: &ValueString, _authority: private::Derivation) -> Self {
        Self::with_map_key(self, key)
    }

    fn module_argument_placeholder(_name: String, _authority: private::Derivation) -> Option<Self> {
        None
    }

    fn binding_in_scope(
        name: RelativeBindingName,
        scope: Option<&str>,
        _authority: private::Derivation,
    ) -> Self {
        let root = super::RootScope::new();
        match scope {
            Some(scope) => root
                .resolve(RelativeBindingName::new(scope))
                .with_attribute(name.as_str()),
            None => root.resolve(name),
        }
    }
}

impl BindingAddress for AbsoluteBindingAddress {
    fn as_str(&self) -> &str {
        self.as_str()
    }
}

pub(crate) fn binding_with_index<B: BindingAddress>(binding: &B, index: i64) -> B {
    <B as private::Sealed>::with_index(binding, index, private::Derivation)
}

pub(crate) fn knowledge_diagnostic<B: BindingAddress>(
    error: super::knowledge::KnowledgeError<B>,
) -> super::knowledge::KnowledgeDiagnostic {
    <B as private::Sealed>::knowledge_diagnostic(error)
}

pub(crate) fn input_preview<P: super::ValueKind, B: BindingAddress>(
    value: super::Value<P, B>,
) -> super::Value<P> {
    <B as private::Sealed>::input_preview(value)
}

pub(crate) fn deferred_template_binding<B: BindingAddress>(binding: &B) -> B {
    <B as private::Sealed>::deferred_template(binding, private::Derivation)
}

pub(crate) fn binding_with_map_key<B: BindingAddress>(binding: &B, key: &ValueString) -> B {
    <B as private::Sealed>::with_map_key(binding, key, private::Derivation)
}

pub(crate) fn module_argument_placeholder<B: BindingAddress>(name: String) -> Option<B> {
    <B as private::Sealed>::module_argument_placeholder(name, private::Derivation)
}

pub(crate) fn binding_in_scope<B: BindingAddress>(
    name: RelativeBindingName,
    scope: Option<&str>,
) -> B {
    <B as private::Sealed>::binding_in_scope(name, scope, private::Derivation)
}

impl PartialEq<str> for RelativeBindingName {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for RelativeBindingName {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<RelativeBindingName> for str {
    fn eq(&self, other: &RelativeBindingName) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<RelativeBindingName> for &str {
    fn eq(&self, other: &RelativeBindingName) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<String> for RelativeBindingName {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<RelativeBindingName> for String {
    fn eq(&self, other: &RelativeBindingName) -> bool {
        self == other.as_str()
    }
}

impl From<RelativeBindingName> for String {
    fn from(value: RelativeBindingName) -> Self {
        value.into_string()
    }
}

impl From<AbsoluteBindingAddress> for String {
    fn from(value: AbsoluteBindingAddress) -> Self {
        value.into_string()
    }
}

pub(crate) fn binding_selected_attribute<B: BindingAddress>(binding: &B, attribute: &str) -> B {
    <B as private::Sealed>::selected_attribute(binding, attribute, private::Derivation)
}
pub(crate) fn binding_collection<B: BindingAddress>(binding: &B, index: usize) -> B {
    <B as private::Sealed>::collection_binding(binding, index, private::Derivation)
}

#[cfg(test)]
mod tests {
    use super::AbsoluteBindingAddress;

    #[test]
    fn for_value_keys_membership_opaque_quoted_keys() {
        for key in ["a']x", "a[b].c", "a'b", "a\\", ""] {
            let spelling = crate::utils::map_key_address("rs", key);
            let checked = super::decompose_binding_address(&spelling)
                .unwrap_or_else(|| panic!("existing key spelling must be recognized: {spelling}"));
            assert!(checked.is_direct_child_of("rs"), "{spelling}");
            assert_eq!(checked.candidate_ancestors().collect::<Vec<_>>(), ["rs"]);
        }
    }

    #[test]
    fn persisted_absolute_binding_address_rejects_empty_spelling() {
        let error = serde_json::from_str::<AbsoluteBindingAddress>(r#""""#)
            .expect_err("an empty persisted binding address must be rejected");

        assert!(
            error
                .to_string()
                .contains("an absolute binding address cannot be empty"),
            "unexpected deserialization error: {error}",
        );
    }
}
