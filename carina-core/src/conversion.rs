//! Checked conversion diagnostics and pure declaration policy.
//! Construction is private to the authored conversion policy and completion.
pub(crate) use crate::parser::retained::conversion::diagnostic::ConversionTypePath;
pub(crate) use crate::parser::retained::conversion::diagnostic::{
    ConversionChannel, ConversionFindings,
};
pub use crate::parser::retained::conversion::diagnostic::{ConversionError, ConversionErrorKind};

#[cfg(test)]
mod cursor_policy_tests;
#[cfg(test)]
pub(crate) mod tests;
