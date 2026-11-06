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

impl<A: InsideSecret, B: InsideSecret> InsideSecret for (A, B) {
    fn inside_secret(self) -> Self {
        (self.0.inside_secret(), self.1.inside_secret())
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

/// A diagnostic boundary is always explicit; persistence fallbacks use Secret.
///
/// ```compile_fail
/// use carina_core::resource::DiagnosticScope;
/// let _: DiagnosticScope = Default::default();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DiagnosticScope {
    Public,
    Secret,
}

impl DiagnosticScope {
    pub fn is_public(&self) -> bool {
        *self == Self::Public
    }
    pub fn is_secret(self) -> bool {
        self == Self::Secret
    }
    pub fn path<B: super::BindingAddress>(
        self,
        path: impl Into<Option<super::ReferencePath<B>>>,
    ) -> DiagnosticPath<B> {
        match self {
            Self::Public => DiagnosticPath {
                public: path.into(),
            },
            Self::Secret => DiagnosticPath { public: None },
        }
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

/// A path whose raw identity is retained only when its scope permits rendering.
/// Only [`DiagnosticScope::path`] can construct one.
///
/// ```compile_fail
/// use carina_core::resource::{DiagnosticPath, ReferencePath, RelativeBindingName};
/// let path: DiagnosticPath<RelativeBindingName> = DiagnosticPath::Public(ReferencePath::Binding("private".into()));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiagnosticPath<B: super::BindingAddress = super::AbsoluteBindingAddress> {
    public: Option<super::ReferencePath<B>>,
}

impl<B: super::BindingAddress> InsideSecret for DiagnosticPath<B> {
    fn inside_secret(self) -> Self {
        DiagnosticScope::Secret.path(None)
    }
}

impl<B: super::BindingAddress> DiagnosticPath<B> {
    pub fn is_secret(&self) -> bool {
        self.public.is_none()
    }

    /// Render only a public identity; a protected identity was discarded at construction.
    pub fn render<T>(
        &self,
        public: impl FnOnce(&super::ReferencePath<B>) -> T,
        secret: impl FnOnce() -> T,
    ) -> T {
        match &self.public {
            Some(path) => public(path),
            None => secret(),
        }
    }

    pub fn to_dot_string(&self) -> String {
        self.render(super::ReferencePath::to_dot_string, || {
            crate::schema::SECRET_VALUE_MASK.into()
        })
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
