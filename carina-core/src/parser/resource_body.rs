//! Classify direct resource children once, before parsing or collecting locations.

use pest::iterators::{Pair, Pairs};
use std::{collections::BTreeMap, path::Path};

use super::{DeclarationLocation, Located, PreprocessedFile, Rule};
use crate::provider_text::DiagnosticText;
use crate::resource::ResourceDirective;
use crate::validation::ResourceDirectiveFinding;

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
    pub(super) fn classification<'a>(
        &self,
        classified: &'a ClassifiedResourceDirectives,
    ) -> &'a DirectiveBlock {
        classified
            .blocks
            .get(&self.name.as_span().start())
            .expect("syntax inventory classifies every directives block")
    }

    /// Evaluation and provenance consume the exact classification that owns
    /// the finding. Neither consumer decodes or classifies keys again.
    pub(super) fn entries<'a>(
        self,
        classified: &'a ClassifiedResourceDirectives,
    ) -> impl Iterator<Item = (Pair<'i, Rule>, &'a DirectiveEntry)> {
        self.unclassified_entries().map(|entry| {
            let classification = classified
                .entries
                .get(&entry.as_span().start())
                .expect("syntax inventory classifies every directive entry");
            (entry, classification)
        })
    }

    fn unclassified_entries(self) -> impl Iterator<Item = Pair<'i, Rule>> {
        self.contents.map(|pair| {
            if pair.as_rule() == Rule::block_content {
                pair.into_inner().next().expect("directive content")
            } else {
                pair
            }
        })
    }
}

pub(super) enum DirectiveBlock {
    First,
    Duplicate(Located<ResourceDirectiveFinding>),
}

pub(super) enum DirectiveEntry {
    Valid(ResourceDirective),
    Invalid(Located<ResourceDirectiveFinding>),
}

/// CST classifications live only for the parse. Their located findings are
/// retained in ParserBindingMetadata through every subsequent file phase.
#[derive(Default)]
pub(super) struct ClassifiedResourceDirectives {
    blocks: BTreeMap<usize, DirectiveBlock>,
    entries: BTreeMap<usize, DirectiveEntry>,
}

impl ClassifiedResourceDirectives {
    pub(super) fn from_pairs(
        pairs: Pairs<'_, Rule>,
        file: &PreprocessedFile,
        source: &Path,
    ) -> Self {
        let mut classified = Self::default();
        for body in pairs.flatten().filter_map(ResourceBody::from_pair) {
            let (_, contents) = body.into_parts();
            for content in contents {
                if let BlockContent::Directives(block) = content {
                    let classification = match &block.first {
                        None => DirectiveBlock::First,
                        Some(first) => DirectiveBlock::Duplicate(Located::new(
                            DeclarationLocation::from_pair(&block.name, file, source),
                            ResourceDirectiveFinding::DuplicateBlock {
                                first_line: DeclarationLocation::from_pair(first, file, source)
                                    .span()
                                    .start_line(),
                            },
                        )),
                    };
                    classified
                        .blocks
                        .insert(block.name.as_span().start(), classification);
                    for entry in block.unclassified_entries() {
                        let classification = match entry.as_rule() {
                            Rule::attribute => {
                                let key = entry.clone().into_inner().next().expect("directive key");
                                let name = super::AttributeKey::literal_in_file(key.clone(), file);
                                match ResourceDirective::ALL
                                    .iter()
                                    .find(|(known, _)| name.matches(known))
                                {
                                    Some((_, directive)) => DirectiveEntry::Valid(*directive),
                                    None => {
                                        let known: Vec<_> = ResourceDirective::ALL
                                            .iter()
                                            .map(|(name, _)| *name)
                                            .collect();
                                        let suggestion = crate::schema::suggest_similar_name(
                                            &name.to_string(),
                                            &known,
                                            &[],
                                        );
                                        DirectiveEntry::Invalid(Located::new(
                                            DeclarationLocation::from_pair(&key, file, source),
                                            ResourceDirectiveFinding::UnknownKey {
                                                name: DiagnosticText::new(name.to_string()),
                                                suggestion,
                                            },
                                        ))
                                    }
                                }
                            }
                            Rule::nested_block => {
                                let name = entry
                                    .clone()
                                    .into_inner()
                                    .next()
                                    .expect("invalid directive block name");
                                DirectiveEntry::Invalid(Located::new(
                                    DeclarationLocation::from_pair(&name, file, source),
                                    ResourceDirectiveFinding::InvalidEntry,
                                ))
                            }
                            _ => DirectiveEntry::Invalid(Located::new(
                                DeclarationLocation::from_pair(&entry, file, source),
                                ResourceDirectiveFinding::InvalidEntry,
                            )),
                        };
                        classified
                            .entries
                            .insert(entry.as_span().start(), classification);
                    }
                }
            }
        }
        classified
    }

    pub(super) fn findings(&self) -> Vec<Located<ResourceDirectiveFinding>> {
        let mut findings = Vec::new();
        for block in self.blocks.values() {
            match block {
                DirectiveBlock::First => {}
                DirectiveBlock::Duplicate(finding) => findings.push(finding.clone()),
            }
        }
        for entry in self.entries.values() {
            match entry {
                DirectiveEntry::Valid(_directive) => {}
                DirectiveEntry::Invalid(finding) => findings.push(finding.clone()),
            }
        }
        findings.sort_by_key(|finding| finding.location().span().start_byte());
        findings
    }
}
