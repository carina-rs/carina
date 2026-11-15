//! Attach identity findings to syntax already validated by the analysis view.
use super::*;
use crate::parser::{AnalysisTree, DeclarationLocation, DeferredForPhaseTraversal, Located};
use crate::resource::ParserPlaceholder;

type IdentityAnalysis<'a, D, B> = AnalysisTree<'a, ParserPlaceholder, ParserPlaceholder, D, B>;

pub fn attribute_derived_conflict_diagnostics<'a, D, B: BindingAddress>(
    tree: &IdentityAnalysis<'_, D, B>,
    conflicts: &'a [AttributeDerivedAnonymousResourceConflict],
    schemas: &SchemaRegistry,
) -> Vec<Located<&'a AttributeDerivedAnonymousResourceConflict>>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let mut findings = Vec::new();
    for item in tree.resources() {
        for conflict in conflicts
            .iter()
            .filter(|conflict| item.has_attribute_derived_conflict(conflict, schemas))
        {
            let header = tree
                .syntax()
                .anonymous_headers()
                .iter()
                .find(|header| {
                    header.file() == item.location().file()
                        && item.location().span().contains(header.span())
                })
                .expect("anonymous declaration header validated by parser inventory");
            findings.push(Located::new(header.clone(), conflict));
        }
    }
    findings
}

pub fn anonymous_identity_diagnostics<D, B: BindingAddress>(
    tree: &IdentityAnalysis<'_, D, B>,
    error: AnonymousIdentityError,
    root: &Path,
) -> Vec<Located<String>>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let mut anchors: Vec<&DeclarationLocation> = tree
        .syntax()
        .loops()
        .iter()
        .chain(tree.syntax().anonymous_headers())
        .collect();
    anchors.extend(
        tree.module_calls()
            .map(|call| call.id().declaration_location()),
    );
    for item in tree.resources() {
        anchors.extend(item.module_identity_locations());
    }
    let source_anchor = |file: &Path, span: SourceSpan| {
        let full = if file.is_absolute() {
            file.to_path_buf()
        } else {
            root.join(file)
        };
        *anchors
            .iter()
            .find(|location| {
                (location.file().path() == file || location.file().path() == full)
                    && location.span() == span
            })
            .expect("identity source must retain its parser-owned occurrence")
    };
    let mut findings = Vec::new();
    for error in error.into_individual_errors() {
        let message = error.to_string();
        let members = match &error {
            AnonymousIdentityError::IncompleteDeclaredIdentity(error) => error.members(),
            AnonymousIdentityError::Collision(error) => error.members(),
            AnonymousIdentityError::Loop(error) => {
                let span = error.source_span().expect("live loop identity source span");
                findings.push(Located::new(
                    source_anchor(Path::new(error.file()), span).clone(),
                    message,
                ));
                continue;
            }
            // These two policies have their own source-specific producers.
            AnonymousIdentityError::AttributeDerived(_)
            | AnonymousIdentityError::LegacyMoveTarget(_) => continue,
            AnonymousIdentityError::Aggregate(_) => {
                unreachable!("identity aggregates were flattened")
            }
        };
        for member in members {
            for source in member.source_locations() {
                findings.push(Located::new(
                    source_anchor(source.file(), source.span()).clone(),
                    message.clone(),
                ));
            }
            if member.source_locations().is_empty() {
                let item = tree
                    .resources()
                    .find(|item| item.id() == member.resource_id())
                    .expect("identity member belongs to the sealed graph");
                let header = tree
                    .syntax()
                    .anonymous_headers()
                    .iter()
                    .find(|header| {
                        header.file() == item.location().file()
                            && item.location().span().contains(header.span())
                    })
                    .expect("anonymous identity member has its parser-owned header");
                findings.push(Located::new(header.clone(), message.clone()));
            }
        }
    }
    let mut seen = HashSet::new();
    findings.retain(|finding| seen.insert((finding.location().clone(), finding.detail().clone())));
    findings
}

pub fn legacy_loop_move_target_diagnostics<D, B: BindingAddress>(
    tree: &IdentityAnalysis<'_, D, B>,
    errors: Vec<LegacyLoopMoveTargetError>,
) -> Vec<Located<LegacyLoopMoveTargetError>>
where
    D: DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    errors
        .into_iter()
        .map(|error| {
            let span = error.source_span().expect("parsed state destination span");
            let location = tree
                .syntax()
                .state_targets()
                .iter()
                .find(|location| location.span() == span)
                .expect("state destination retained in parser inventory");
            Located::new(location.clone(), error)
        })
        .collect()
}
