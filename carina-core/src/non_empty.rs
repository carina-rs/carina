//! A vector that is statically guaranteed to contain at least one
//! element. Encoding the invariant in the type lets readers skip
//! defensive `is_empty` checks and forces empty handling at the
//! conversion boundary instead of at every read site.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmptyVec<T> {
    // Invariant: never empty. Maintained by every constructor.
    inner: Vec<T>,
}

impl<T> NonEmptyVec<T> {
    pub fn from_vec(v: Vec<T>) -> Option<Self> {
        if v.is_empty() {
            None
        } else {
            Some(Self { inner: v })
        }
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.inner.iter()
    }

    pub fn as_slice(&self) -> &[T] {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_vec_empty_returns_none() {
        assert!(NonEmptyVec::<i32>::from_vec(Vec::new()).is_none());
    }

    #[test]
    fn from_vec_preserves_order() {
        let nev = NonEmptyVec::from_vec(vec![1, 2, 3]).unwrap();
        let collected: Vec<_> = nev.iter().copied().collect();
        assert_eq!(collected, vec![1, 2, 3]);
        assert_eq!(nev.as_slice(), &[1, 2, 3]);
    }
}
