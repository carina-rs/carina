use std::fmt;
use std::hash::Hash;

use serde::{Deserialize, Deserializer, Serialize};

use super::ValueString;

mod private {
    pub trait Sealed {}
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

    pub fn canonicalize_persisted_map_keys(&self) -> Self {
        Self(ValueString::derived_from(
            crate::utils::canonicalize_map_key_address(self.as_str()),
            [&self.0],
        ))
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
#[cfg(any(test, feature = "test-support"))]
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
    fn as_str(&self) -> &str;

    fn diagnostic_literal(&self) -> Option<&str>;

    #[doc(hidden)]
    fn with_index(&self, index: i64) -> Self;

    #[doc(hidden)]
    fn with_map_key(&self, key: &ValueString) -> Self;

    #[doc(hidden)]
    fn module_argument_placeholder(name: String) -> Option<Self>;
}

impl private::Sealed for RelativeBindingName {}

impl BindingAddress for RelativeBindingName {
    fn as_str(&self) -> &str {
        self.as_str()
    }

    fn diagnostic_literal(&self) -> Option<&str> {
        self.0.diagnostic_literal()
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

    fn module_argument_placeholder(name: String) -> Option<Self> {
        Some(Self::new(name))
    }
}

impl private::Sealed for AbsoluteBindingAddress {}

impl BindingAddress for AbsoluteBindingAddress {
    fn as_str(&self) -> &str {
        self.as_str()
    }

    fn diagnostic_literal(&self) -> Option<&str> {
        self.0.diagnostic_literal()
    }

    fn with_index(&self, index: i64) -> Self {
        Self::with_index(self, index)
    }

    fn with_map_key(&self, key: &ValueString) -> Self {
        Self::with_map_key(self, key)
    }

    fn module_argument_placeholder(_name: String) -> Option<Self> {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::AbsoluteBindingAddress;

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
