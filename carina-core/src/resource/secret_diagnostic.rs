//! Protection carried by diagnostic data, independently of reference identity.
use std::fmt;

pub const INSIDE_SECRET_MESSAGE: &str = "invalid value inside secret(...)";

/// Error payloads must implement this transformation to cross a secret boundary.
pub trait InsideSecret: Sized {
    fn inside_secret(self) -> Self;
}

impl InsideSecret for std::convert::Infallible {
    fn inside_secret(self) -> Self {
        match self {}
    }
}

impl InsideSecret for String {
    fn inside_secret(self) -> Self {
        INSIDE_SECRET_MESSAGE.into()
    }
}

impl<E: InsideSecret> InsideSecret for Vec<E> {
    fn inside_secret(self) -> Self {
        self.into_iter().map(InsideSecret::inside_secret).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SecretDiagnosticError<E> {
    Public(E),
    InsideSecret,
}

impl<E> InsideSecret for SecretDiagnosticError<E> {
    fn inside_secret(self) -> Self {
        Self::InsideSecret
    }
}

impl<E: fmt::Display> fmt::Display for SecretDiagnosticError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Public(error) => error.fmt(f),
            Self::InsideSecret => f.write_str(INSIDE_SECRET_MESSAGE),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticScope {
    Public,
    Secret,
}

impl DiagnosticScope {
    pub fn is_secret(self) -> bool {
        self == Self::Secret
    }
    pub fn protect<E>(self, error: E) -> SecretDiagnosticError<E> {
        match self {
            Self::Public => SecretDiagnosticError::Public(error),
            Self::Secret => SecretDiagnosticError::InsideSecret,
        }
    }
    pub fn protect_error<E: InsideSecret>(self, error: E) -> E {
        match self {
            Self::Public => error,
            Self::Secret => error.inside_secret(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DiagnosticPath<B: super::BindingAddress = super::AbsoluteBindingAddress> {
    Public(super::ReferencePath<B>),
    Protected,
}

impl<B: super::BindingAddress> InsideSecret for DiagnosticPath<B> {
    fn inside_secret(self) -> Self {
        Self::Protected
    }
}

impl<B: super::BindingAddress> DiagnosticPath<B> {
    pub fn to_dot_string(&self) -> String {
        match self {
            Self::Public(path) => path.to_dot_string(),
            Self::Protected => "(secret)".into(),
        }
    }
}

impl InsideSecret for () {
    fn inside_secret(self) -> Self {
        self
    }
}

impl<E: InsideSecret> InsideSecret for Box<E> {
    fn inside_secret(self) -> Self {
        Box::new((*self).inside_secret())
    }
}
