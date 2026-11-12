//! Checked conversion diagnostics and pure declaration policy.
//! Construction is private to the authored conversion policy and completion.
pub(crate) use crate::parser::retained::conversion::diagnostic::ConversionTypePath;
pub use crate::parser::retained::conversion::diagnostic::{
    ConversionChannel, ConversionError, ConversionErrorKind, ConversionFindings,
};

#[cfg(test)]
mod cursor_policy_tests;
#[cfg(test)]
pub(crate) mod tests;
