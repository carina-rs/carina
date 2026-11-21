use std::fmt;
use std::hash::Hash;

use serde::{Deserialize, Deserializer, Serialize};

use super::{ResourceIdentity, ValueString};

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

/// Admission retains each iterable member with the address minted for it.
/// Bound expansion accepts this unit; callers cannot pair an address with a
/// separately resolved iterable.
#[derive(Debug, Clone)]
pub(crate) struct AdmittedForMember<P: super::PlaceholderKind, B: BindingAddress> {
    address: BoundForAddress<B>,
    value: super::Value<P, B>,
    index: Option<i64>,
    key: Option<String>,
}

impl<P: super::PlaceholderKind, B: BindingAddress> AdmittedForMember<P, B> {
    pub(crate) fn address(&self) -> &BoundForAddress<B> {
        &self.address
    }

    pub(crate) fn value(&self) -> &super::Value<P, B> {
        &self.value
    }

    pub(crate) fn index(&self) -> Option<i64> {
        self.index
    }

    pub(crate) fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

fn admitted_member_value<P: super::PlaceholderKind, B: BindingAddress>(
    member: super::knowledge::ClassifiedRef<'_, '_, P, B>,
    moment: ForAddressMoment,
) -> Result<super::Value<P, B>, ForAdmissionFailure<B>> {
    use super::knowledge::{ClassifiedRef, PlanIterable};
    if moment == ForAddressMoment::Parse {
        return Ok(member.diagnostic_value().clone());
    }
    Ok(match member {
        ClassifiedRef::Known(value) => value.to_value(),
        ClassifiedRef::Shape(value) => PlanIterable::from_shape(value.clone())?.for_expansion(),
        ClassifiedRef::Pending(value) => value.to_value(),
        ClassifiedRef::Failed(value) => return Err(value.error().into()),
    })
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
    #[error("Element {element} cannot be used as an address key. Use `for (i, x) in ...`.")]
    NonRoundTrippingKey { element: usize },
    #[error("Element {element} is not known before apply. Use `for (i, x) in ...`.")]
    NotKnownBeforeApply { element: usize },
    #[error("Element {element} is still unknown during expansion. Use `for (i, x) in ...`.")]
    UnknownDuringExpansion { element: usize },
    #[error(
        "Element type {kind} cannot be a value key. Rewrite as strings or use `for (i, x) in ...`."
    )]
    ElementType { kind: &'static str },
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

/// A deferred decision is control flow, never a key error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForAddressDefer {
    phase: super::knowledge::Phase,
    element: Option<usize>,
}

impl ForAddressDefer {
    pub(crate) fn element(&self) -> Option<usize> {
        self.element
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForAddressMoment {
    Parse,
    Validation,
    Plan,
    Apply,
}

#[derive(Debug)]
pub(crate) enum ForAddressAdmission<P: super::PlaceholderKind, B: BindingAddress> {
    Admitted(Vec<AdmittedForMember<P, B>>),
    Defer(ForAddressDefer),
    Reject(ForKeyError),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ForAdmissionFailure<B: BindingAddress> {
    #[error(transparent)]
    Shape(#[from] crate::parser::ShapeMismatch),
    #[error(transparent)]
    Knowledge(#[from] super::knowledge::KnowledgeError<B>),
}

/// Every bound expansion crosses this three-way decision before deferring or
/// obtaining child-address authority. Protection and knowledge failures are
/// resolved here, before collection-shape dispatch at any consumer.
pub(crate) fn admit_bound_for_addresses<P: super::PlaceholderKind, B: BindingAddress>(
    addressing: crate::parser::LoopAddressing,
    target: &crate::parser::BoundDeferredTarget<B>,
    members: super::knowledge::ClassifiedRef<'_, '_, P, B>,
    moment: ForAddressMoment,
) -> Result<ForAddressAdmission<P, B>, ForAdmissionFailure<B>> {
    use super::ConcreteValueRef;
    use super::knowledge::{ClassifiedRef, Phase};
    use crate::parser::LoopAddressing;
    use ForAddressAdmission::{Admitted, Defer, Reject};
    let base = target.binding_address();
    if addressing == LoopAddressing::ValueKeyed && members.is_protected() {
        return Ok(Reject(ForKeyError::Protected));
    }
    match members {
        ClassifiedRef::Pending(pending) => {
            return Ok(Defer(ForAddressDefer {
                phase: pending.phase(),
                element: None,
            }));
        }
        ClassifiedRef::Failed(failure) => return Err(failure.error().into()),
        ClassifiedRef::Known(_) | ClassifiedRef::Shape(_) => {}
    }
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
    let shape =
        |expected| ForAdmissionFailure::Shape(crate::parser::ShapeMismatch::new(expected, got));
    match addressing {
        LoopAddressing::ValueKeyed => {
            enum Member<B: BindingAddress> {
                Key(ValueKey, super::ValueString),
                Pending(ForAddressDefer),
                Reject(ForKeyError),
                Failed(super::knowledge::KnowledgeError<B>),
            }
            let items: Vec<Member<B>> = if let Some(items) = members.list() {
                items
                    .into_iter()
                    .enumerate()
                    .map(|(index, item)| {
                        let element = index + 1;
                        if item.is_protected() {
                            return Member::Reject(ForKeyError::Protected);
                        }
                        match item.concrete() {
                            Some(ConcreteValueRef::String(value)) => {
                                match ValueKey::from_string(value) {
                                    Ok(key) => Member::Key(key, value.clone()),
                                    Err(error) => Member::Reject(error),
                                }
                            }
                            Some(_) => Member::Reject(ForKeyError::NonString { element }),
                            None => match item {
                                ClassifiedRef::Pending(pending) => {
                                    Member::Pending(ForAddressDefer {
                                        phase: pending.phase(),
                                        element: Some(element),
                                    })
                                }
                                ClassifiedRef::Failed(failure) => Member::Failed(failure.error()),
                                ClassifiedRef::Shape(_) | ClassifiedRef::Known(_) => {
                                    Member::Reject(ForKeyError::NonString { element })
                                }
                            },
                        }
                    })
                    .collect()
            } else if let Some(ConcreteValueRef::StringList(strings)) = members.concrete() {
                strings
                    .iter()
                    .map(|value| match ValueKey::from_string(value) {
                        Ok(key) => Member::Key(key, value.clone()),
                        Err(error) => Member::Reject(error),
                    })
                    .collect()
            } else {
                return Err(shape("list"));
            };
            if items
                .iter()
                .any(|item| matches!(item, Member::Reject(ForKeyError::Protected)))
            {
                return Ok(Reject(ForKeyError::Protected));
            }
            // Failed evaluation is evidence of failure, never a pending key.
            if let Some(error) = items.iter().find_map(|item| match item {
                Member::Failed(error) => Some(error.clone()),
                _ => None,
            }) {
                return Err(error.into());
            }
            if moment == ForAddressMoment::Parse
                && let Some(pending) = items.iter().find_map(|item| match item {
                    Member::Pending(pending) => Some(*pending),
                    _ => None,
                })
            {
                return Ok(Defer(pending));
            }
            let mut seen = std::collections::BTreeMap::new();
            let mut keys = Vec::with_capacity(items.len());
            let mut pending = None;
            for (index, item) in items.into_iter().enumerate() {
                let element = index + 1;
                let (key, value) = match item {
                    Member::Key(key, value) => (key, value),
                    Member::Reject(error) => return Ok(Reject(error)),
                    Member::Failed(error) => return Err(error.into()),
                    Member::Pending(waiting) => {
                        match (moment, waiting.phase) {
                            (ForAddressMoment::Apply, _) => {
                                return Ok(Reject(ForKeyError::UnknownDuringExpansion { element }));
                            }
                            (ForAddressMoment::Plan, Phase::Apply) => {
                                return Ok(Reject(ForKeyError::NotKnownBeforeApply { element }));
                            }
                            (ForAddressMoment::Parse | ForAddressMoment::Validation, _)
                            | (ForAddressMoment::Plan, Phase::Plan) => {
                                if pending.is_none() {
                                    pending = Some(waiting);
                                }
                            }
                        }
                        continue;
                    }
                };
                if let Some(first) = seen.insert(key.0.clone(), element) {
                    return Ok(Reject(ForKeyError::Duplicate {
                        first,
                        second: element,
                    }));
                }
                let minted = binding_with_map_key(base, &key.0.clone().into());
                let canonical = crate::utils::canonicalize_map_key_address(minted.as_str());
                if decompose_binding_address(&canonical)
                    .and_then(|address| address.direct_key_of(base.as_str()))
                    != Some(key.0.as_str())
                {
                    return Ok(Reject(ForKeyError::NonRoundTrippingKey { element }));
                }
                keys.push((key, value));
            }
            if let Some(pending) = pending {
                return Ok(Defer(pending));
            }
            Ok(Admitted(
                keys.into_iter()
                    .map(|(key, value)| AdmittedForMember {
                        address: BoundForAddress {
                            binding: binding_with_map_key(base, &key.0.into()),
                        },
                        value: super::Value::Concrete(super::ConcreteValue::String(value)),
                        index: None,
                        key: None,
                    })
                    .collect(),
            ))
        }
        LoopAddressing::Indexed => {
            let items = if let Some(items) = members.list() {
                items
                    .into_iter()
                    .map(|item| admitted_member_value(item, moment))
                    .collect::<Result<Vec<_>, _>>()?
            } else if let Some(ConcreteValueRef::StringList(strings)) = members.concrete() {
                strings
                    .iter()
                    .cloned()
                    .map(|value| super::Value::Concrete(super::ConcreteValue::String(value)))
                    .collect()
            } else {
                return Err(shape("list"));
            };
            Ok(Admitted(
                items
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| {
                        let index = index
                            .try_into()
                            .expect("loop index fits in signed address domain");
                        AdmittedForMember {
                            address: BoundForAddress {
                                binding: binding_with_index(base, index),
                            },
                            value,
                            index: Some(index),
                            key: None,
                        }
                    })
                    .collect(),
            ))
        }
        LoopAddressing::MapKeyed => {
            let items = members.map().ok_or_else(|| shape("map"))?;
            let mut items: Vec<_> = items.into_iter().collect();
            items.sort_by_key(|(key, _)| *key);
            Ok(Admitted(
                items
                    .into_iter()
                    .map(|(key, value)| {
                        Ok(AdmittedForMember {
                            address: BoundForAddress {
                                binding: binding_with_map_key(base, &key.into()),
                            },
                            value: admitted_member_value(value, moment)?,
                            index: None,
                            key: Some(key.to_owned()),
                        })
                    })
                    .collect::<Result<_, ForAdmissionFailure<B>>>()?,
            ))
        }
    }
}

/// A checked decomposition of an existing binding spelling. This grants no
/// authority to construct a new resource address.
#[derive(Debug)]
pub struct CheckedBindingAddress<'a> {
    spelling: &'a str,
    ends: Vec<usize>,
}

impl<'a> CheckedBindingAddress<'a> {
    pub fn ends_in_subscript(&self) -> bool {
        self.ends.len() > 1 && self.spelling.as_bytes()[self.ends[self.ends.len() - 2]] == b'['
    }

    pub fn first_field_prefix(&self) -> Option<(&'a str, &'a str)> {
        let end = self.ends[0];
        (self.spelling.as_bytes().get(end) == Some(&b'.'))
            .then(|| (&self.spelling[..end], &self.spelling[end + 1..]))
    }

    pub fn direct_key_of(&self, parent: &str) -> Option<&'a str> {
        if !self.is_direct_child_of(parent) {
            return None;
        }
        let suffix = &self.spelling[parent.len()..];
        if let Some(key) = suffix.strip_prefix('.') {
            return Some(key);
        }
        suffix
            .strip_prefix("['")
            .and_then(|key| key.strip_suffix("']"))
            .or_else(|| {
                suffix
                    .strip_prefix("[\"")
                    .and_then(|key| key.strip_suffix("\"]"))
            })
    }

    pub fn candidate_ancestors(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.ends[..self.ends.len() - 1]
            .iter()
            .map(|&end| &self.spelling[..end])
    }

    pub fn is_direct_child_of(&self, parent: &str) -> bool {
        self.ends.len() > 1
            && &self.spelling[..self.ends[self.ends.len() - 2]] == parent
            && &self.spelling[parent.len()..] != "[?]"
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
    if root_end == 0 || spelling[..root_end].ends_with(']') {
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
                // Deferred templates are existing internal addresses too.
                Some(b'?') => (bytes.get(cursor + 2) == Some(&b']') && next[cursor + 3].is_some())
                    .then_some(cursor + 3),
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

    fn with_index(&self, index: i64) -> Self {
        Self(ValueString::derived_from(
            format!("{}[{index}]", self.as_str()),
            [&self.0],
        ))
    }

    fn with_map_key(&self, key: &ValueString) -> Self {
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

/// Reference lookup spelling. The child-address minting functions are private
/// to this module; this wrapper does not enforce provenance on Resource.binding.
/// In-crate reference and origin views can still expose the underlying binding.
#[derive(Clone)]
pub(crate) struct BindingLookup<B: BindingAddress> {
    binding: B,
}

impl<B: BindingAddress> BindingLookup<B> {
    pub(crate) fn new(binding: &B) -> Self {
        Self {
            binding: binding.clone(),
        }
    }

    pub(crate) fn select(mut self, index: &super::Subscript) -> Self {
        self.binding = match index {
            super::Subscript::Int { index } => binding_with_index(&self.binding, *index),
            super::Subscript::Str { key } => binding_with_map_key(&self.binding, key),
        };
        self
    }

    pub(crate) fn step(mut self, step: &super::PathSegment) -> Self {
        match step {
            super::PathSegment::Subscript { index } => self.select(index),
            super::PathSegment::Field { name } => {
                self.binding = binding_selected_attribute(&self.binding, name);
                self
            }
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        self.binding.as_str()
    }

    pub(crate) fn reference<P: super::ValueKind>(self) -> super::Value<P, B> {
        super::Value::Deferred(super::DeferredValue::BindingRef {
            binding: self.binding,
        })
    }

    pub(crate) fn resource_reference<P: super::ValueKind>(
        self,
        attribute: String,
        fields: Vec<String>,
        subscripts: Vec<super::Subscript>,
    ) -> super::Value<P, B> {
        super::Value::Deferred(super::DeferredValue::ResourceRef {
            path: super::AccessPath::from_binding(
                self.binding,
                attribute,
                fields
                    .into_iter()
                    .map(|name| super::PathSegment::Field { name })
                    .chain(
                        subscripts
                            .into_iter()
                            .map(|index| super::PathSegment::Subscript { index }),
                    )
                    .collect(),
            ),
        })
    }

    pub(crate) fn record_origin(
        self,
        location: crate::parser::DeclarationLocation,
        origins: &mut Vec<crate::parser::Located<B>>,
    ) {
        origins.push(crate::parser::Located::new(location, self.binding));
    }

    pub(crate) fn dependency(&self) -> super::DependencyKey<B> {
        super::DependencyKey::new(self.binding.clone())
    }
}

fn binding_with_index<B: BindingAddress>(binding: &B, index: i64) -> B {
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

fn binding_with_map_key<B: BindingAddress>(binding: &B, key: &ValueString) -> B {
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

#[cfg(test)]
mod tests {
    use super::AbsoluteBindingAddress;

    #[test]
    fn for_value_keys_membership_excludes_deferred_template_addresses() {
        for spelling in ["rs[0]", "rs[12]", "rs.prod", "rs['?']", "rs['[?]']"] {
            let address = super::decompose_binding_address(spelling).unwrap();
            assert!(address.is_direct_child_of("rs"), "{spelling}");
        }
        for spelling in ["rs[?]", "rs", "rs[0].nested", "rs_suffix[0]"] {
            let address = super::decompose_binding_address(spelling).unwrap();
            assert!(!address.is_direct_child_of("rs"), "{spelling}");
        }
        let template = super::decompose_binding_address("m.rs[?]").unwrap();
        assert!(template.ends_in_subscript());
        assert_eq!(template.first_field_prefix(), Some(("m", "rs[?]")));
        assert!(!template.is_direct_child_of("m.rs"));
    }

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
