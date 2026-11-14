//! Classify direct resource children once, before parsing or collecting locations.

use pest::iterators::{Pair, Pairs};

use super::{DeclarationLocation, ParseError, Rule};

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

pub(super) struct ResourceDirectives<'i> {
    name: Pair<'i, Rule>,
    contents: Pairs<'i, Rule>,
    first: Option<Pair<'i, Rule>>,
}

impl<'i> ResourceDirectives<'i> {
    pub(super) fn ensure_unique(
        &self,
        locate: impl Fn(&Pair<'i, Rule>) -> DeclarationLocation,
    ) -> Result<(), ParseError> {
        if let Some(first) = &self.first {
            let location = locate(&self.name);
            return Err(ParseError::DuplicateDirectives {
                first_line: locate(first).span().start_line(),
            }
            .at(&location));
        }
        Ok(())
    }

    pub(super) fn into_contents(self) -> Pairs<'i, Rule> {
        self.contents
    }

    pub(super) fn attributes(self) -> impl Iterator<Item = Pair<'i, Rule>> {
        self.contents
            .map(|pair| {
                if pair.as_rule() == Rule::block_content {
                    pair.into_inner().next().expect("directive content")
                } else {
                    pair
                }
            })
            .filter(|pair| pair.as_rule() == Rule::attribute)
    }
}
