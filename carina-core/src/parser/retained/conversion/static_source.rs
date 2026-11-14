//! Static source legality shared by the Judge and authored destination walk.
//! These are completed judgements, not obligations or a list of uses to visit.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceKnowledge {
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
pub(in crate::parser::retained) struct StaticVerdict {
    pub(in crate::parser::retained) outcome: Result<TypeExpr, ConversionError>,
    pub(super) matched_destination: bool,
    union_destination: bool,
    site: ConversionSite,
    secret: bool,
    binding: Option<Arc<StaticSourceReceipt>>,
    declaration: DeclarationAddress,
    resolved: DeclarationAddress,
    members: Arc<[(DeclarationAddress, bool)]>,
}
impl StaticVerdict {
    pub(in crate::parser::retained) fn set_binding_receipt(
        &mut self,
        receipt: Arc<StaticSourceReceipt>,
    ) {
        self.binding = Some(receipt);
    }
    pub(super) fn consume<K: DestinationKind>(
        mut self,
        path: &LeafPath,
        knowledge: SourceKnowledge,
        mut origin: VerdictOrigin,
    ) -> ConversionVerdict {
        if self.outcome.is_ok()
            && let Err(error) =
                deferred_union_check(&self.site, self.secret, self.union_destination, knowledge)
        {
            self.outcome = Err(error);
            // This Current-value verdict was not produced by the static Judge.
            origin = VerdictOrigin::Walk;
        }
        let discarded = knowledge == SourceKnowledge::Discarded;
        let decision = match self.outcome {
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
            if !selected && verdict.outcome.is_ok() {
                verdict.outcome = Err(ConversionError::new(
                    Kind::Destination,
                    &self.site.location,
                    "conversion destination must have exactly one matching type identity",
                )
                .owned_by(self.site.node));
                if self.secret {
                    verdict.outcome = verdict.outcome.map_err(InsideSecret::inside_secret);
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
    bindings: Vec<Arc<StaticSourceReceipt>>,
}
#[derive(Debug)]
struct RecordedStaticVerdict {
    verdict: Arc<StaticVerdict>,
    member: Option<bool>,
}
impl RecordedStaticVerdicts {
    fn insert(&mut self, verdict: StaticVerdict) {
        if let Some(binding) = &verdict.binding {
            self.bindings.push(binding.clone());
        }
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
        config: &crate::parser::ProviderContext,
        knowledge: SourceKnowledge,
    ) -> StaticVerdict {
        judge_conversion(
            &self.site,
            &self.target,
            source,
            declaration,
            defs,
            config,
            knowledge,
        )
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
    pub(in crate::parser::retained) fn binding_receipt(
        &self,
        expected: &TypeExpr,
    ) -> Option<Arc<StaticSourceReceipt>> {
        match &*self
            .authority
            .lock()
            .expect("conversion static judgement lock")
        {
            StaticAuthority::WalkOwned => None,
            StaticAuthority::JudgedStatically(verdicts) => verdicts
                .bindings
                .iter()
                .find(|receipt| receipt.matches(expected))
                .cloned(),
        }
    }
    pub(in crate::parser::retained) fn source(&self) -> &JudgedExpr {
        &self.source
    }
    pub(in crate::parser::retained) fn compute(
        &self,
        declaration: &CompiledAttributeType,
        defs: &std::collections::BTreeMap<String, CompiledAttributeType>,
        config: &crate::parser::ProviderContext,
    ) -> StaticVerdict {
        judge_conversion(
            &self.site,
            &self.target,
            &self.source,
            declaration,
            defs,
            config,
            SourceKnowledge::StaticOnly,
        )
    }
    pub(in crate::parser::retained) fn record(
        &self,
        verdict: StaticVerdict,
    ) -> crate::ConversionChannel {
        let mut findings = crate::ConversionChannel::default();
        if let Err(error) = &verdict.outcome
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

pub(super) fn judge_conversion(
    site: &ConversionSite,
    path: &ConversionTypePath,
    source: &JudgedExpr,
    declaration: &CompiledAttributeType,
    defs: &std::collections::BTreeMap<String, CompiledAttributeType>,
    config: &crate::parser::ProviderContext,
    knowledge: SourceKnowledge,
) -> StaticVerdict {
    let mut matched_destination = false;
    let mut union_destination = false;
    let mut resolved_address = DeclarationAddress::of(declaration);
    let mut probes = Vec::new();
    let outcome = (|| {
        let target = diagnostic::resolve_target(path, &site.location, config)
            .map_err(|error| error.owned_by(site.node))?;
        let resolved =
            crate::schema::value_validation::resolve_assignment_declaration(declaration, defs)
                .map_err(|cause| ConversionError::destination(&site.location, site.node, cause))?;
        resolved_address = DeclarationAddress::of(resolved);
        union_destination = matches!(resolved.kind(), AttrTypeKind::Union(_));
        let selected = destination(resolved, &target, defs).map_err(|kind| {
            static_error(
                site,
                source,
                kind,
                "conversion destination must have exactly one matching type identity",
            )
        })?;
        matched_destination = true;
        if let Some(members) = crate::schema::union_members_with_defs(resolved, defs) {
            for (index, member) in members.iter().enumerate() {
                if let Some(member) = member {
                    let selected = std::ptr::eq(member.as_attr(), selected);
                    probes.push((DeclarationAddress::of(member.as_attr()), selected));
                    // Preserve the raw wrapper's consumption stamp too. Identity
                    // selection above uses the definition-aware member only.
                    if let crate::schema::RawShape::Union(raw) = resolved.raw_shape()
                        && let Some(raw) = raw.get(index)
                    {
                        probes.push((DeclarationAddress::of(raw), selected));
                    }
                }
            }
        }
        let discarded = knowledge == SourceKnowledge::Discarded;
        let message = if discarded {
            "discarded conversion operand has an incompatible scalar kind"
        } else {
            "conversion operand has no compatible scalar source judgement"
        };
        let source_type = check_scalar_kind(selected, source)
            .map_err(|kind| static_error(site, source, kind, message))?;
        deferred_union_check(
            site,
            site.parsing.secret || source.diagnostic_is_secret(),
            union_destination,
            knowledge,
        )?;
        if !discarded && let TypeExpr::SchemaType { owned } = source_type {
            let source_declaration = diagnostic::source_declaration(owned, &site.location, config)
                .map_err(|error| error.owned_by(site.node))?;
            if let Some(bounds) = DeclarationBounds::from_declaration(selected)
                && DeclarationBounds::from_declaration(&source_declaration)
                    .is_some_and(|source| source.disjoint(&bounds))
            {
                return Err(static_error(
                    site,
                    source,
                    Kind::Disjoint,
                    "conversion source and destination declaration bounds are disjoint",
                ));
            }
        }
        Ok(TypeExpr::SchemaType { owned: target })
    })()
    .map_err(|error: ConversionError| {
        if site.parsing.secret || source.diagnostic_is_secret() {
            error.inside_secret()
        } else {
            error
        }
    });
    StaticVerdict {
        outcome,
        matched_destination,
        union_destination,
        site: site.clone(),
        secret: site.parsing.secret || source.diagnostic_is_secret(),
        binding: None,
        declaration: DeclarationAddress::of(declaration),
        resolved: resolved_address,
        members: probes.into(),
    }
}
