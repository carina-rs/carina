//! Ordered declarations and their immutable name index are minted together.
use super::{PatternForm, PatternStructField};
use std::{collections::HashMap, fmt, ops::Deref, sync::Arc};

#[derive(Clone)]
pub struct IndexedStructFields<R: PatternForm> {
    ordered: Vec<PatternStructField<R>>,
    names: Arc<FieldNames>,
}

#[derive(Default)]
struct FieldNames {
    canonical: HashMap<String, (usize, usize)>,
    aliases: HashMap<String, Vec<usize>>,
}

impl<R: PatternForm> IndexedStructFields<R> {
    pub fn field(&self, name: &str) -> Option<&PatternStructField<R>> {
        #[cfg(test)]
        crate::input_rules::record_declaration_lookup_work(1);
        self.names
            .canonical
            .get(name)
            .map(|(first, _)| &self.ordered[*first])
    }
    pub(crate) fn authored_field(
        &self,
        name: &str,
        alias_is_active: impl Fn(&PatternStructField<R>) -> bool,
    ) -> Option<&PatternStructField<R>> {
        #[cfg(test)]
        crate::input_rules::record_declaration_lookup_work(1);
        let canonical = self.names.canonical.get(name).map(|(_, last)| *last);
        let alias = self.names.aliases.get(name).and_then(|indices| {
            indices
                .iter()
                .rev()
                .copied()
                .find(|index| alias_is_active(&self.ordered[*index]))
        });
        canonical
            .into_iter()
            .chain(alias)
            .max()
            .map(|index| &self.ordered[index])
    }
    pub(crate) fn first_authored_field(
        &self,
        name: &str,
        alias_is_active: impl Fn(&PatternStructField<R>) -> bool,
    ) -> Option<&PatternStructField<R>> {
        #[cfg(test)]
        crate::input_rules::record_declaration_lookup_work(1);
        let canonical = self.names.canonical.get(name).map(|(first, _)| *first);
        let alias = self.names.aliases.get(name).and_then(|indices| {
            indices
                .iter()
                .copied()
                .find(|index| alias_is_active(&self.ordered[*index]))
        });
        canonical
            .into_iter()
            .chain(alias)
            .min()
            .map(|index| &self.ordered[index])
    }
    pub(crate) fn as_slice(&self) -> &[PatternStructField<R>] {
        &self.ordered
    }
}

impl<R: PatternForm> From<Vec<PatternStructField<R>>> for IndexedStructFields<R> {
    fn from(ordered: Vec<PatternStructField<R>>) -> Self {
        let mut names = FieldNames::default();
        for (index, field) in ordered.iter().enumerate() {
            // Admission rejects duplicates; until then retain the first match,
            // exactly as the former ordered declaration search did.
            names
                .canonical
                .entry(field.name.clone())
                .and_modify(|(_, last)| *last = index)
                .or_insert((index, index));
            if let Some(alias) = &field.block_name {
                names.aliases.entry(alias.clone()).or_default().push(index);
            }
        }
        Self {
            ordered,
            names: Arc::new(names),
        }
    }
}
impl<R: PatternForm> FromIterator<PatternStructField<R>> for IndexedStructFields<R> {
    fn from_iter<T: IntoIterator<Item = PatternStructField<R>>>(values: T) -> Self {
        Self::from(values.into_iter().collect::<Vec<_>>())
    }
}
impl<R: PatternForm> Default for IndexedStructFields<R> {
    fn default() -> Self {
        Self::from(Vec::new())
    }
}
impl<R: PatternForm> Deref for IndexedStructFields<R> {
    type Target = [PatternStructField<R>];
    fn deref(&self) -> &Self::Target {
        &self.ordered
    }
}
impl<R: PatternForm> IntoIterator for IndexedStructFields<R> {
    type Item = PatternStructField<R>;
    type IntoIter = std::vec::IntoIter<PatternStructField<R>>;
    fn into_iter(self) -> Self::IntoIter {
        self.ordered.into_iter()
    }
}
impl<'a, R: PatternForm> IntoIterator for &'a IndexedStructFields<R> {
    type Item = &'a PatternStructField<R>;
    type IntoIter = std::slice::Iter<'a, PatternStructField<R>>;
    fn into_iter(self) -> Self::IntoIter {
        self.ordered.iter()
    }
}
impl<R: PatternForm> fmt::Debug for IndexedStructFields<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.ordered.fmt(f)
    }
}
