//! Classify resource directives before exposing their entries to consumers.

use pest::iterators::{Pair, Pairs};
use std::path::Path;

use super::{DeclarationLocation, Located, PreprocessedFile, Rule};
use crate::provider_text::DiagnosticText;
use crate::resource::ResourceDirective;
use crate::validation::ResourceDirectiveFinding;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AuthoredSlotKind {
    Attribute,
    Directives,
}

/// Classify metadata blocks for resource evaluation and retained slots.
/// Conversion placement follows its own unchanged syntax policy.
pub(super) fn resource_child_kind(pair: &Pair<'_, Rule>) -> AuthoredSlotKind {
    let child = if pair.as_rule() == Rule::block_content {
        pair.clone().into_inner().next().expect("resource content")
    } else {
        pair.clone()
    };
    if child.as_rule() == Rule::nested_block
        && child
            .into_inner()
            .next()
            .expect("nested block name")
            .as_str()
            == "directives"
    {
        AuthoredSlotKind::Directives
    } else {
        AuthoredSlotKind::Attribute
    }
}

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

    pub(super) fn into_parts(
        self,
        file: &PreprocessedFile,
        source: &Path,
    ) -> (Pair<'i, Rule>, BlockContents<'i>) {
        (
            self.resource_type,
            BlockContents::resource(self.contents, file, source),
        )
    }
}

pub(super) enum BlockContents<'i> {
    Ordinary(Pairs<'i, Rule>),
    Resource(std::vec::IntoIter<BlockContent<'i>>),
}

pub(super) enum BlockContent<'i> {
    Ordinary(Pair<'i, Rule>),
    Directives(ResourceDirectives<'i>),
}

impl<'i> BlockContents<'i> {
    pub(super) fn ordinary(pairs: Pairs<'i, Rule>) -> Self {
        Self::Ordinary(pairs)
    }

    fn resource(pairs: Pairs<'i, Rule>, file: &PreprocessedFile, source: &Path) -> Self {
        let mut first_directives = None;
        let contents = pairs
            .map(|pair| {
                if pair.as_rule() == Rule::block_content {
                    let block = pair.clone().into_inner().next().expect("resource content");
                    if block.as_rule() == Rule::nested_block {
                        let mut contents = block.into_inner();
                        let name = contents.next().expect("nested block name");
                        if resource_child_kind(&pair) == AuthoredSlotKind::Directives {
                            let location = DeclarationLocation::from_pair(&name, file, source);
                            let block = ResourceDirectives::classify(
                                location.clone(),
                                contents,
                                first_directives.as_ref(),
                                file,
                                source,
                            );
                            first_directives.get_or_insert(location);
                            return BlockContent::Directives(block);
                        }
                    }
                }
                BlockContent::Ordinary(pair)
            })
            .collect::<Vec<_>>();
        Self::Resource(contents.into_iter())
    }
}

impl<'i> Iterator for BlockContents<'i> {
    type Item = BlockContent<'i>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Ordinary(pairs) => pairs.next().map(BlockContent::Ordinary),
            Self::Resource(contents) => contents.next(),
        }
    }
}

/// A directives block cannot expose an entry without its classification.
/// Pre-checks borrow the already classified entries before evaluation.
pub(super) struct ResourceDirectives<'i> {
    classification: DirectiveBlock,
    entries: Vec<(Pair<'i, Rule>, DirectiveEntry)>,
}

impl<'i> ResourceDirectives<'i> {
    pub(super) fn classification(&self) -> &DirectiveBlock {
        &self.classification
    }

    pub(super) fn entries(&self) -> impl Iterator<Item = (Pair<'i, Rule>, &DirectiveEntry)> {
        self.entries
            .iter()
            .map(|(pair, entry)| (pair.clone(), entry))
    }

    /// The sole owner of directive classification, shared by syntax inventory,
    /// evaluation and element provenance. The classification travels with the
    /// CST entry rather than depending on a separately initialized context.
    fn classify(
        location: DeclarationLocation,
        contents: Pairs<'i, Rule>,
        first: Option<&DeclarationLocation>,
        file: &PreprocessedFile,
        source: &Path,
    ) -> Self {
        let classification = match first {
            None => DirectiveBlock::First,
            Some(first) => DirectiveBlock::Duplicate(Located::new(
                location,
                ResourceDirectiveFinding::DuplicateBlock {
                    first_line: first.span().start_line(),
                },
            )),
        };
        let entries = contents
            .map(|pair| {
                let entry = if pair.as_rule() == Rule::block_content {
                    pair.into_inner().next().expect("directive content")
                } else {
                    pair
                };
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
                (entry, classification)
            })
            .collect();
        Self {
            classification,
            entries,
        }
    }

    fn collect_findings(&self, findings: &mut Vec<Located<ResourceDirectiveFinding>>) {
        match self.classification() {
            DirectiveBlock::First => {}
            DirectiveBlock::Duplicate(finding) => findings.push(finding.clone()),
        }
        for (_, entry) in self.entries() {
            match entry {
                DirectiveEntry::Valid(_directive) => {}
                DirectiveEntry::Invalid(finding) => findings.push(finding.clone()),
            }
        }
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

/// Retain the findings owned by classified CST entries, including resources
/// that evaluation will not visit (for example, an empty loop body).
pub(super) fn directive_findings(
    pairs: Pairs<'_, Rule>,
    file: &PreprocessedFile,
    source: &Path,
) -> Vec<Located<ResourceDirectiveFinding>> {
    let mut findings = Vec::new();
    for body in pairs.flatten().filter_map(ResourceBody::from_pair) {
        let (_, contents) = body.into_parts(file, source);
        for content in contents {
            match content {
                BlockContent::Ordinary(_pair) => {}
                BlockContent::Directives(block) => block.collect_findings(&mut findings),
            }
        }
    }
    findings.sort_by_key(|finding| finding.location().span().start_byte());
    findings
}
