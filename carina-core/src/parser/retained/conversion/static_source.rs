//! Static source legality shared by the Judge and authored destination walk.
//! These are completed judgements, not obligations or a list of uses to visit.
use super::*;
use crate::parser::ResolvedSchemaType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::parser::retained) enum SourceKnowledge {
    StaticOnly,
    Known,
    Deferred,
    Discarded,
}
impl SourceKnowledge {
    pub(super) fn at<P: crate::resource::PlaceholderKind, B: BindingAddress>(
        checked: CheckValue<'_, P, B>,
        source: &JudgedExpr,
    ) -> Self {
        match checked {
            CheckValue::Discarded => Self::Discarded,
            CheckValue::Current(Value::Concrete(_)) => Self::Known,
            CheckValue::Current(Value::Deferred(_)) | CheckValue::Pending | CheckValue::Covered => {
                if source.concrete() {
                    Self::Known
                } else {
                    Self::Deferred
                }
            }
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DeclarationAddress(usize);
impl DeclarationAddress {
    fn of(declaration: &CompiledAttributeType) -> Self {
        Self(std::ptr::from_ref(declaration).addr())
    }
}

#[derive(Debug, Clone)]
enum SourceCheck {
    Judged(Result<(), ConversionError>),
    WalkOwned,
}

#[derive(Debug, Clone)]
struct MatchedDestination {
    target: ResolvedSchemaType,
    selected: CompiledAttributeType,
}

#[derive(Debug, Clone)]
pub(in crate::parser::retained) struct StaticVerdict {
    destination: Result<MatchedDestination, ConversionError>,
    pub(super) matched_destination: bool,
    union_destination: bool,
    site: ConversionSite,
    secret: bool,
    declaration: DeclarationAddress,
    resolved: DeclarationAddress,
    members: Arc<[(DeclarationAddress, bool)]>,
    source_check: SourceCheck,
}
impl StaticVerdict {
    pub(in crate::parser::retained) fn outcome(&self) -> Result<TypeExpr, ConversionError> {
        let destination = self.destination.as_ref().map_err(Clone::clone)?;
        match &self.source_check {
            SourceCheck::Judged(Err(error)) => Err(error.clone()),
            SourceCheck::Judged(Ok(())) | SourceCheck::WalkOwned => Ok(TypeExpr::SchemaType {
                owned: destination.target.clone(),
            }),
        }
    }
    pub(in crate::parser::retained) fn source_is_walk_owned(&self) -> bool {
        matches!(self.source_check, SourceCheck::WalkOwned)
    }
    pub(in crate::parser::retained) fn with_source(
        mut self,
        source: &JudgedExpr,
        context: &ConversionContext<'_>,
        knowledge: SourceKnowledge,
    ) -> Self {
        if let Ok(destination) = &self.destination {
            self.source_check = judge_source(
                &self.site,
                source,
                destination,
                context,
                knowledge,
                self.union_destination,
            );
        }
        self
    }
    /// A Judge-owned destination is never resolved or selected a second time.
    /// Only the source relation deferred by that Judge use can run here.
    pub(super) fn resume<K: AssignmentKind>(
        mut self,
        path: &LeafPath,
        knowledge: SourceKnowledge,
        source: &JudgedExpr,
        context: &ConversionContext<'_>,
    ) -> ConversionVerdict {
        let origin = if self.destination.is_ok() && self.source_is_walk_owned() {
            self = self.with_source(source, context, knowledge);
            VerdictOrigin::Walk
        } else {
            VerdictOrigin::Judge
        };
        self.consume::<K>(path, knowledge, origin)
    }
    pub(super) fn consume<K: AssignmentKind>(
        mut self,
        path: &LeafPath,
        knowledge: SourceKnowledge,
        mut origin: VerdictOrigin,
    ) -> ConversionVerdict {
        if self.outcome().is_ok()
            && let Err(error) =
                deferred_union_check(&self.site, self.secret, self.union_destination, knowledge)
        {
            self.source_check = SourceCheck::Judged(Err(error));
            // This Current-value verdict was not produced by the static Judge.
            origin = VerdictOrigin::Walk;
        }
        let discarded = knowledge == SourceKnowledge::Discarded;
        let decision = match self.outcome() {
            Ok(_) => {
                let mut findings = if discarded {
                    AssignmentFindings::default()
                } else {
                    crate::validation::admission::conversion_local::<K>(path)
                };
                findings.matched_destination = true;
                Decision::Accepted(
                    findings,
                    AcceptedConversion {
                        node: self.site.node,
                        location: self.site.location,
                        secret: self.secret,
                    },
                )
            }
            Err(error) => Decision::Rejected(AssignmentFindings {
                conversions: match origin {
                    VerdictOrigin::Judge => Default::default(),
                    VerdictOrigin::Walk => std::iter::once(error).collect(),
                },
                matched_destination: self.matched_destination,
                claim_effect: crate::schema::value_validation::ClaimEffect::Rejected,
                ..Default::default()
            }),
        };
        ConversionVerdict(Box::new(decision))
    }
    fn for_member(&self, member: Option<bool>) -> Self {
        let mut verdict = self.clone();
        if let Some(selected) = member {
            verdict.matched_destination = selected;
            if !selected && verdict.outcome().is_ok() {
                verdict.destination = Err(ConversionError::new(
                    Kind::Destination,
                    &self.site.location,
                    "conversion destination must have exactly one matching type identity",
                )
                .owned_by(self.site.node));
                if self.secret {
                    verdict.destination = verdict.destination.map_err(InsideSecret::inside_secret);
                }
            }
        }
        verdict
    }
}

/// A Convert owns its operand evidence and completed static results. The
/// authority is closed: a walk may compute only a WalkOwned source; a recorded
/// Judge result is read, including the member selection for union probes.
#[derive(Debug)]
pub(in crate::parser::retained) struct ConversionTypeJudgement {
    site: ConversionSite,
    target: ConversionTypePath,
    source: JudgedExpr,
    bare: bool,
    authority: std::sync::Mutex<StaticAuthority>,
    reported: std::sync::OnceLock<ConversionError>,
}
#[derive(Debug)]
enum StaticAuthority {
    WalkOwned,
    JudgedStatically(RecordedStaticVerdicts),
}
#[derive(Debug, Default)]
struct RecordedStaticVerdicts {
    // Completed results on this Convert, keyed by the consuming declaration's
    // stamp. No syntax/value correspondence; never enumerated for diagnostics.
    declarations: std::collections::HashMap<DeclarationAddress, RecordedStaticVerdict>,
}
#[derive(Debug)]
struct RecordedStaticVerdict {
    verdict: Arc<StaticVerdict>,
    member: Option<bool>,
}
impl RecordedStaticVerdicts {
    fn insert(&mut self, verdict: StaticVerdict) {
        let verdict = Arc::new(verdict);
        for address in [verdict.declaration, verdict.resolved] {
            self.declarations.insert(
                address,
                RecordedStaticVerdict {
                    verdict: verdict.clone(),
                    member: None,
                },
            );
        }
        for (address, selected) in verdict.members.iter() {
            self.declarations.insert(
                *address,
                RecordedStaticVerdict {
                    verdict: verdict.clone(),
                    member: Some(*selected),
                },
            );
        }
    }
}
pub(super) enum VerdictOrigin {
    Judge,
    Walk,
}
pub(super) enum StaticOwner {
    WalkOwned,
    JudgedStatically(Box<StaticVerdict>),
    Missing(ConversionError),
}
impl ConversionTypeJudgement {
    pub(in crate::parser::retained) fn new(
        site: &ConversionSite,
        target: &ConversionTypePath,
        source: JudgedExpr,
        bare: bool,
    ) -> Self {
        Self {
            site: site.clone(),
            target: target.clone(),
            source,
            bare,
            authority: std::sync::Mutex::new(StaticAuthority::WalkOwned),
            reported: std::sync::OnceLock::new(),
        }
    }
    pub(super) fn check_walk(
        &self,
        source: &JudgedExpr,
        declaration: &CompiledAttributeType,
        defs: &std::collections::BTreeMap<String, CompiledAttributeType>,
        context: &ConversionContext<'_>,
        knowledge: SourceKnowledge,
    ) -> StaticVerdict {
        judge_destination(
            &self.site,
            &self.target,
            source,
            declaration,
            defs,
            context.catalog,
        )
        .with_source(source, context, knowledge)
    }
    pub(super) fn owner(&self, declaration: &CompiledAttributeType) -> StaticOwner {
        match &*self
            .authority
            .lock()
            .expect("conversion static judgement lock")
        {
            StaticAuthority::WalkOwned => StaticOwner::WalkOwned,
            StaticAuthority::JudgedStatically(verdicts) => {
                match verdicts
                    .declarations
                    .get(&DeclarationAddress::of(declaration))
                {
                    Some(cached) => StaticOwner::JudgedStatically(Box::new(
                        cached.verdict.for_member(cached.member),
                    )),
                    None => StaticOwner::Missing(self.error(
                        Kind::Source,
                        "conversion static judgement belongs to another destination",
                    )),
                }
            }
        }
    }
    pub(in crate::parser::retained) fn is_bare(&self) -> bool {
        self.bare
    }
    pub(in crate::parser::retained) fn source(&self) -> &JudgedExpr {
        &self.source
    }
    pub(in crate::parser::retained) fn compute_destination(
        &self,
        declaration: &CompiledAttributeType,
        defs: &std::collections::BTreeMap<String, CompiledAttributeType>,
        config: &crate::parser::ProviderContext,
    ) -> StaticVerdict {
        judge_destination(
            &self.site,
            &self.target,
            &self.source,
            declaration,
            defs,
            config,
        )
    }
    pub(in crate::parser::retained) fn record(
        &self,
        verdict: StaticVerdict,
    ) -> crate::ConversionChannel {
        let mut findings = crate::ConversionChannel::default();
        if let Err(error) = verdict.outcome()
            && self.reported.set(error.clone()).is_ok()
        {
            findings = std::iter::once(error.clone()).collect();
        }
        let mut authority = self
            .authority
            .lock()
            .expect("conversion static judgement lock");
        match &mut *authority {
            StaticAuthority::WalkOwned => {
                let mut verdicts = RecordedStaticVerdicts::default();
                verdicts.insert(verdict);
                *authority = StaticAuthority::JudgedStatically(verdicts);
            }
            StaticAuthority::JudgedStatically(verdicts) => verdicts.insert(verdict),
        }
        findings
    }
    fn error(&self, kind: Kind, message: &str) -> ConversionError {
        static_error(&self.site, &self.source, kind, message)
    }
}

/// Knownness belongs to the Current/Discarded value policy, not template typing.
fn deferred_union_check(
    site: &ConversionSite,
    secret: bool,
    union: bool,
    knowledge: SourceKnowledge,
) -> Result<(), ConversionError> {
    if union && knowledge == SourceKnowledge::Deferred {
        let error = ConversionError::new(
            Kind::UnsupportedPosition,
            &site.location,
            "conversion of an unknown value under a union destination is not supported yet",
        )
        .owned_by(site.node);
        Err(if secret { error.inside_secret() } else { error })
    } else {
        Ok(())
    }
}

fn static_error(
    site: &ConversionSite,
    source: &JudgedExpr,
    kind: Kind,
    message: &str,
) -> ConversionError {
    let error = ConversionError::new(kind, &site.location, message).owned_by(site.node);
    if site.parsing.secret || source.diagnostic_is_secret() {
        error.inside_secret()
    } else {
        error
    }
}

fn judge_destination(
    site: &ConversionSite,
    path: &ConversionTypePath,
    source: &JudgedExpr,
    declaration: &CompiledAttributeType,
    defs: &std::collections::BTreeMap<String, CompiledAttributeType>,
    config: &crate::parser::ProviderContext,
) -> StaticVerdict {
    let mut matched_destination = false;
    let mut union_destination = false;
    let mut resolved_address = DeclarationAddress::of(declaration);
    let mut probes = Vec::new();
    let mut selected_address = None;
    let resolved =
        crate::schema::value_validation::resolve_assignment_declaration(declaration, defs);
    // Cache all consumption stamps even when the target/identity is rejected.
    // A union probe then reads that Judge failure rather than inventing a miss.
    if let Ok(resolved) = &resolved {
        resolved_address = DeclarationAddress::of(resolved);
        union_destination = matches!(resolved.kind(), AttrTypeKind::Union(_));
        if let Some(members) = crate::schema::union_members_with_defs(resolved, defs) {
            for (index, member) in members.iter().enumerate() {
                if let Some(member) = member {
                    let canonical = DeclarationAddress::of(member.as_attr());
                    probes.push((canonical, canonical));
                    if let crate::schema::RawShape::Union(raw) = resolved.raw_shape()
                        && let Some(raw) = raw.get(index)
                    {
                        probes.push((DeclarationAddress::of(raw), canonical));
                    }
                }
            }
        }
    }
    let destination = (|| {
        let target = diagnostic::resolve_target(path, &site.location, config)
            .map_err(|error| error.owned_by(site.node))?;
        let resolved = resolved
            .map_err(|cause| ConversionError::destination(&site.location, site.node, cause))?;
        let selected = destination(resolved, &target, defs).map_err(|kind| {
            static_error(
                site,
                source,
                kind,
                "conversion destination must have exactly one matching type identity",
            )
        })?;
        matched_destination = true;
        selected_address = Some(DeclarationAddress::of(selected));
        Ok(MatchedDestination {
            target,
            selected: selected.clone(),
        })
    })()
    .map_err(|error: ConversionError| {
        if site.parsing.secret || source.diagnostic_is_secret() {
            error.inside_secret()
        } else {
            error
        }
    });
    StaticVerdict {
        destination,
        matched_destination,
        union_destination,
        site: site.clone(),
        secret: site.parsing.secret || source.diagnostic_is_secret(),
        declaration: DeclarationAddress::of(declaration),
        resolved: resolved_address,
        members: probes
            .into_iter()
            .map(|(address, canonical)| (address, Some(canonical) == selected_address))
            .collect(),
        source_check: SourceCheck::WalkOwned,
    }
}

fn judge_source(
    site: &ConversionSite,
    source: &JudgedExpr,
    destination: &MatchedDestination,
    context: &ConversionContext<'_>,
    knowledge: SourceKnowledge,
    union_destination: bool,
) -> SourceCheck {
    let selected = &destination.selected;
    let config = context.catalog;
    let mut static_source = true;
    let result = (|| {
        let discarded = knowledge == SourceKnowledge::Discarded;
        let check_deferred = || {
            deferred_union_check(
                site,
                site.parsing.secret || source.diagnostic_is_secret(),
                union_destination,
                knowledge,
            )
        };
        let mut check_source = |evidence| {
            match check_scalar_kind(selected, evidence) {
                ScalarKindCheck::Compatible(scalar) => {
                    check_deferred()?;
                    if !discarded {
                        let source_bounds = match scalar {
                            CompatibleScalar::Declaration(source) => {
                                DeclarationBounds::from_declaration(source.as_attr())
                            }
                            CompatibleScalar::Judged(source_type) => {
                                if let TypeExpr::SchemaType { owned } = source_type {
                                    let declaration = diagnostic::source_declaration(
                                        owned,
                                        &site.location,
                                        config,
                                    )
                                    .map_err(|error| error.owned_by(site.node))?;
                                    DeclarationBounds::from_declaration(&declaration)
                                } else {
                                    None
                                }
                            }
                        };
                        if let Some(bounds) = DeclarationBounds::from_declaration(selected)
                            && source_bounds.is_some_and(|source| source.disjoint(&bounds))
                        {
                            return Err(static_error(
                                site,
                                source,
                                Kind::Disjoint,
                                "conversion source and destination declaration bounds are disjoint",
                            ));
                        }
                    }
                }
                ScalarKindCheck::WrongScalarKind => {
                    return Err(static_error(
                        site,
                        source,
                        Kind::ScalarKind,
                        "conversion operand has an incompatible scalar kind",
                    ));
                }
                ScalarKindCheck::NotScalar => {
                    return Err(static_error(
                        site,
                        source,
                        Kind::NotScalar,
                        "conversion operand is not a scalar value",
                    ));
                }
                ScalarKindCheck::NoStaticJudgement => {
                    static_source = false;
                    check_deferred()?;
                }
            }
            Ok(())
        };
        match &context.reference {
            crate::validation::admission::ReferenceSource::Unchecked => {
                check_source(ScalarSource::Judged(source))?;
            }
            crate::validation::admission::ReferenceSource::Typed(reference) => {
                for declaration in reference.constraints() {
                    check_source(ScalarSource::Declaration(declaration))?;
                }
            }
        };
        Ok(())
    })()
    .map_err(|error: ConversionError| {
        if site.parsing.secret || source.diagnostic_is_secret() {
            error.inside_secret()
        } else {
            error
        }
    });
    match result {
        Err(error) => SourceCheck::Judged(Err(error)),
        Ok(()) if static_source => SourceCheck::Judged(Ok(())),
        Ok(()) => SourceCheck::WalkOwned,
    }
}
