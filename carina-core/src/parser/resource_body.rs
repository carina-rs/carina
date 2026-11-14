//! Classify direct resource children once, before parsing or collecting locations.

use pest::iterators::{Pair, Pairs};

use super::Rule;

pub(super) struct ResourceBody<'i> {
    resource_type: Pair<'i, Rule>,
    contents: Pairs<'i, Rule>,
}

impl<'i> ResourceBody<'i> {
    pub(super) fn from_pair(pair: Pair<'i, Rule>) -> Option<Self> {
        if !matches!(
            pair.as_rule(),
            Rule::resource_expr | Rule::anonymous_resource | Rule::read_resource_expr
        ) {
            return None;
        }
        let mut contents = pair.into_inner();
        let resource_type = contents.next().expect("resource grammar has a type");
        Some(Self {
            resource_type,
            contents,
        })
    }

    pub(super) fn into_parts(self) -> (Pair<'i, Rule>, BlockContents<'i>) {
        (
            self.resource_type,
            BlockContents {
                pairs: self.contents,
                position: BodyPosition::Resource,
                first_directives: None,
            },
        )
    }
}

#[derive(PartialEq, Eq)]
enum BodyPosition {
    Resource,
    Ordinary,
}

pub(super) struct BlockContents<'i> {
    pairs: Pairs<'i, Rule>,
    position: BodyPosition,
    first_directives: Option<Pair<'i, Rule>>,
}

pub(super) enum BlockContent<'i> {
    Ordinary(Pair<'i, Rule>),
    Directives(ResourceDirectives<'i>),
}

impl<'i> BlockContents<'i> {
    pub(super) fn ordinary(pairs: Pairs<'i, Rule>) -> Self {
        Self {
            pairs,
            position: BodyPosition::Ordinary,
            first_directives: None,
        }
    }
}

impl<'i> Iterator for BlockContents<'i> {
    type Item = BlockContent<'i>;

    fn next(&mut self) -> Option<Self::Item> {
        let pair = self.pairs.next()?;
        if self.position == BodyPosition::Resource && pair.as_rule() == Rule::block_content {
            let block = pair.clone().into_inner().next().expect("resource content");
            if block.as_rule() == Rule::nested_block {
                let mut contents = block.into_inner();
                let name = contents.next().expect("nested block name");
                if name.as_str() == "directives" {
                    let first = self.first_directives.clone();
                    self.first_directives.get_or_insert_with(|| name.clone());
                    return Some(BlockContent::Directives(ResourceDirectives {
                        name,
                        contents,
                        first,
                    }));
                }
            }
        }
        Some(BlockContent::Ordinary(pair))
    }
}

#[derive(Clone)]
pub(super) struct ResourceDirectives<'i> {
    name: Pair<'i, Rule>,
    contents: Pairs<'i, Rule>,
    first: Option<Pair<'i, Rule>>,
}

impl<'i> ResourceDirectives<'i> {
    pub(super) fn duplicate(&self) -> Option<(&Pair<'i, Rule>, &Pair<'i, Rule>)> {
        self.first.as_ref().map(|first| (&self.name, first))
    }

    /// Every child must be handled explicitly, including malformed entries.
    pub(super) fn entries(self) -> impl Iterator<Item = DirectiveEntry<'i>> {
        self.contents.map(|pair| {
            let entry = if pair.as_rule() == Rule::block_content {
                pair.into_inner().next().expect("directive content")
            } else {
                pair
            };
            match entry.as_rule() {
                Rule::attribute => DirectiveEntry::Attribute(entry),
                Rule::nested_block => DirectiveEntry::Invalid(
                    entry
                        .into_inner()
                        .next()
                        .expect("invalid directive block name"),
                ),
                _ => DirectiveEntry::Invalid(entry),
            }
        })
    }
}

pub(super) enum DirectiveEntry<'i> {
    Attribute(Pair<'i, Rule>),
    /// The block name or complete non-assignment entry to diagnose.
    Invalid(Pair<'i, Rule>),
}
