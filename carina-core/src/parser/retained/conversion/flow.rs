//! Conversion provenance belongs to judgements, never to lowered Values.
use super::*;

/// The value edges which may transport a conversion to an attribute slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportRole {
    Parentheses,
    IfValue,
    CoalesceValue,
    ListElement,
    MapValue,
    StructFieldValue,
    DefinitionValue,
    AliasForward,
    AttributeSlot,
    SequenceResult,
}

/// A boundary supplies its own judgement; it does not inherit a producer's
/// conversion. Its caller/producer is checked by placement before this cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NonCrossedEdge {
    ModuleArgument,
    FunctionParameter,
    SchemaAttributeReference,
    ModuleOutputReference,
    UpstreamStateReference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlacementRole {
    Convert,
    Transport(TransportRole),
    Computed,
    Boundary,
    SecretArgument,
    Resource,
    Leaf,
}
impl PlacementRole {
    pub(crate) const fn of<B: BindingAddress>(kind: &ExprKind<B>) -> Self {
        use TransportRole as T;
        match kind {
            ExprKind::Convert { .. } => Self::Convert,
            ExprKind::Parens => Self::Transport(T::Parentheses),
            ExprKind::If => Self::Transport(T::IfValue),
            ExprKind::Coalesce => Self::Transport(T::CoalesceValue),
            ExprKind::List => Self::Transport(T::ListElement),
            ExprKind::Map(_) => Self::Transport(T::MapValue),
            ExprKind::NestedBlock { .. } => Self::Transport(T::StructFieldValue),
            ExprKind::Definition => Self::Transport(T::DefinitionValue),
            ExprKind::Binding(_) | ExprKind::Alias => Self::Transport(T::AliasForward),
            ExprKind::Sink { .. } => Self::Transport(T::AttributeSlot),
            ExprKind::Sequence => Self::Transport(T::SequenceResult),
            ExprKind::Function { .. }
            | ExprKind::ModuleCall
            | ExprKind::Declaration
            | ExprKind::ProviderDeclaration => Self::Boundary,
            ExprKind::Secret => Self::SecretArgument,
            ExprKind::Resource { .. } => Self::Resource,
            ExprKind::Scalar(_) | ExprKind::Parameter(_) | ExprKind::UntypedParameter => Self::Leaf,
            ExprKind::Element
            | ExprKind::LoopSlot(_)
            | ExprKind::Reference { .. }
            | ExprKind::Interpolation { .. }
            | ExprKind::EmptyInterpolation
            | ExprKind::Call(_)
            | ExprKind::Access(_)
            | ExprKind::Integer { .. }
            | ExprKind::Iterable
            | ExprKind::For
            | ExprKind::Wait
            | ExprKind::Compose
            | ExprKind::DynamicAccess
            | ExprKind::Boolean
            | ExprKind::RequireCondition(_)
            | ExprKind::Unsupported(_) => Self::Computed,
        }
    }
}

/// The operational export check relies on P1 rejecting authored conversions.
/// B2 must replace that shortcut with a conversion verdict before changing
/// export declarations into transport positions.
pub(crate) struct OperationalExportBoundary(());
pub(crate) const OPERATIONAL_EXPORT_BOUNDARY: OperationalExportBoundary =
    match PlacementRole::of::<RelativeBindingName>(&ExprKind::Declaration) {
        PlacementRole::Boundary => OperationalExportBoundary(()),
        PlacementRole::Convert
        | PlacementRole::Transport(_)
        | PlacementRole::Computed
        | PlacementRole::SecretArgument
        | PlacementRole::Resource
        | PlacementRole::Leaf => {
            panic!("B2 requires a conversion verdict in the pre-apply export check")
        }
    };

/// Persistent value-flow evidence. This is not a set of syntax to complete:
/// only children which contribute to a judgement occur in the evidence.
#[derive(Debug, Clone)]
pub(in crate::parser::retained) struct ConversionSources(Option<Arc<ConversionFlow>>);
#[derive(Debug)]
enum ConversionFlow {
    Convert(Arc<ConversionSite>),
    Joined(Arc<Self>, Arc<Self>),
}
/// Non-empty provenance, obtainable only from the single judgement fact.
#[derive(Clone, Copy)]
pub(in crate::parser::retained) struct CarriedConversion<'a>(&'a ConversionFlow);
impl<'a> CarriedConversion<'a> {
    pub(in crate::parser::retained) fn first(self) -> &'a ConversionSite {
        let mut current = self.0;
        loop {
            match current {
                ConversionFlow::Convert(site) => return site,
                ConversionFlow::Joined(left, _) => current = left,
            }
        }
    }
}

/// Per-Judge expansion memo for the contributing provenance DAG. This is not
/// diagnostic ownership/deduplication: the sole NodeId collector still owns
/// that rule. Strong references prevent allocation addresses being reused.
pub(in crate::parser::retained) struct PlacementFindings {
    findings: crate::ConversionChannel,
    expanded: std::collections::HashMap<usize, Arc<ConversionFlow>>,
}
impl PlacementFindings {
    pub(in crate::parser::retained) fn new(findings: crate::ConversionChannel) -> Self {
        Self {
            findings,
            expanded: Default::default(),
        }
    }
    pub(in crate::parser::retained) fn into_findings(self) -> crate::ConversionChannel {
        self.findings
    }
    pub(in crate::parser::retained) fn take_findings(&mut self) -> crate::ConversionChannel {
        std::mem::take(&mut self.findings)
    }
    pub(in crate::parser::retained) fn append_static(
        &mut self,
        findings: crate::ConversionChannel,
    ) {
        self.findings.append(findings);
    }
    pub(in crate::parser::retained) fn append_rejection(&mut self, rejection: ConversionRejection) {
        self.findings.append(rejection.into_channel());
    }
    fn reject(&mut self, source: Arc<ConversionFlow>, kind: Kind, message: &str, secret: bool) {
        let mut pending = vec![source];
        while let Some(source) = pending.pop() {
            let address = Arc::as_ptr(&source) as usize;
            if self.expanded.insert(address, source.clone()).is_some() {
                continue;
            }
            match source.as_ref() {
                ConversionFlow::Convert(site) => {
                    let error =
                        ConversionError::new(kind, &site.location, message).owned_by(site.node);
                    self.findings.append(
                        std::iter::once(if secret { error.inside_secret() } else { error })
                            .collect(),
                    );
                }
                ConversionFlow::Joined(left, right) => {
                    pending.push(right.clone());
                    pending.push(left.clone());
                }
            }
        }
    }
}
impl ConversionSources {
    pub(in crate::parser::retained) fn empty() -> Self {
        Self(None)
    }
    fn conversion(site: &ConversionSite) -> Self {
        Self(Some(Arc::new(ConversionFlow::Convert(Arc::new(
            site.clone(),
        )))))
    }
    pub(in crate::parser::retained) fn is_empty(&self) -> bool {
        self.0.is_none()
    }
    pub(in crate::parser::retained) fn carried(&self) -> Option<CarriedConversion<'_>> {
        self.0.as_deref().map(CarriedConversion)
    }
    pub(in crate::parser::retained) fn join(self, right: Self) -> Self {
        Self(match (self.0, right.0) {
            (None, right) => right,
            (left, None) => left,
            (Some(left), Some(right)) if Arc::ptr_eq(&left, &right) => Some(left),
            (Some(left), Some(right)) => Some(Arc::new(ConversionFlow::Joined(left, right))),
        })
    }
    pub(in crate::parser::retained) fn first(&self) -> Option<&ConversionSite> {
        self.carried().map(CarriedConversion::first)
    }
    pub(in crate::parser::retained) fn reject(
        &self,
        kind: Kind,
        message: &str,
        secret: bool,
        output: &mut PlacementFindings,
    ) {
        if let Some(source) = &self.0 {
            output.reject(source.clone(), kind, message, secret);
        }
    }
}

/// A reference that has no source judgement cannot transport a converted value.
pub(in crate::parser::retained) enum MissingReferenceEdge {
    UndefinedBinding,
    BindingCycle,
}

/// Every judgement constructor supplies one of these explicit edge decisions.
/// There is no missing/default conversion flag.
pub(in crate::parser::retained) enum JudgementEdges<'a> {
    Leaf,
    Follow {
        inherited: &'a ConversionSources,
        children: &'a [JudgedExpr],
    },
    Selected(&'a JudgedExpr),
    Convert {
        site: &'a ConversionSite,
        target: &'a ConversionTypePath,
        operand: &'a JudgedExpr,
        bare: bool,
    },
    NonCrossed(NonCrossedEdge),
    MissingReference(MissingReferenceEdge),
}
impl JudgementEdges<'_> {
    pub(in crate::parser::retained) fn sources(self) -> ConversionSources {
        match self {
            Self::Leaf => ConversionSources::empty(),
            Self::MissingReference(reason) => match reason {
                MissingReferenceEdge::UndefinedBinding | MissingReferenceEdge::BindingCycle => {
                    ConversionSources::empty()
                }
            },
            Self::Follow {
                inherited,
                children,
            } => children.iter().fold(inherited.clone(), |sources, child| {
                sources.join(child.conversion_sources().clone())
            }),
            Self::Selected(child) => child.conversion_sources().clone(),
            Self::Convert {
                site,
                target: _,
                operand: _,
                bare: _,
            } => ConversionSources::conversion(site),
            Self::NonCrossed(edge) => match edge {
                NonCrossedEdge::ModuleArgument
                | NonCrossedEdge::FunctionParameter
                | NonCrossedEdge::SchemaAttributeReference
                | NonCrossedEdge::ModuleOutputReference
                | NonCrossedEdge::UpstreamStateReference => ConversionSources::empty(),
            },
        }
    }
}

pub(in crate::parser::retained) fn check_placement<B: BindingAddress>(
    root: &AuthoredExpr<ParserPlaceholder, B>,
    children: &[JudgedExpr],
    inherited: &ConversionSources,
    secret: bool,
    chain_owner: Option<&ConversionSite>,
    output: &mut PlacementFindings,
) -> bool {
    let sources = children.iter().fold(inherited.clone(), |sources, child| {
        sources.join(child.conversion_sources().clone())
    });
    let rejection = match root.conversion_role() {
        PlacementRole::Boundary => Some((
            Kind::UnsupportedPosition,
            "conversion is not supported at this position yet",
        )),
        PlacementRole::Computed => Some((
            Kind::UnsupportedPosition,
            "conversion is not supported at this position",
        )),
        PlacementRole::SecretArgument => Some((
            Kind::UnsupportedPosition,
            "conversion is not supported at this position",
        )),
        PlacementRole::Convert if !sources.is_empty() => {
            let site = chain_owner
                .or(root.first_conversion.as_deref())
                .expect("Convert owns its site");
            ConversionSources::conversion(site).reject(
                Kind::UnsupportedPosition,
                "a conversion cannot be the operand of another conversion",
                secret,
                output,
            );
            return true;
        }
        PlacementRole::Transport(TransportRole::IfValue) => {
            if let Some(condition) = root.condition_index().and_then(|index| children.get(index)) {
                condition.conversion_sources().reject(
                    Kind::UnsupportedPosition,
                    "conversion is not supported at this position",
                    secret,
                    output,
                );
                return condition.contains_any_conversion();
            }
            None
        }
        PlacementRole::Transport(TransportRole::AttributeSlot) => {
            if let AuthoredChildRoles::Sink { key: Some(key), .. } = root.roles
                && let Some((index, _)) = root
                    .children
                    .iter()
                    .enumerate()
                    .find(|(_, child)| child.node_id() == key)
                && let Some(key) = children.get(index)
            {
                key.conversion_sources().reject(
                    Kind::UnsupportedPosition,
                    "conversion is not supported at this position",
                    secret,
                    output,
                );
            }
            None
        }
        PlacementRole::Convert
        | PlacementRole::Transport(
            TransportRole::Parentheses
            | TransportRole::CoalesceValue
            | TransportRole::ListElement
            | TransportRole::MapValue
            | TransportRole::StructFieldValue
            | TransportRole::DefinitionValue
            | TransportRole::AliasForward
            | TransportRole::SequenceResult,
        )
        | PlacementRole::Resource
        | PlacementRole::Leaf => None,
    };
    if let Some((kind, message)) = rejection {
        sources.reject(kind, message, secret, output);
        !sources.is_empty()
    } else {
        false
    }
}
/// A rejected authored typed sink cannot become an ordinary error or success.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversionRejection {
    channel: crate::ConversionChannel,
}
impl ConversionRejection {
    pub(in crate::parser::retained) fn at_sink(expression: &JudgedExpr) -> Self {
        let mut output = PlacementFindings::new(Default::default());
        expression.conversion_sources().reject(
            Kind::UnsupportedPosition,
            "conversion is not supported at this position yet",
            expression.diagnostic_is_secret(),
            &mut output,
        );
        Self {
            channel: output.into_findings(),
        }
    }
    pub(crate) fn into_channel(self) -> crate::ConversionChannel {
        self.channel
    }
}
#[derive(Debug, Clone, PartialEq)]
pub enum SinkRejection<E> {
    Ordinary(E),
    Conversion(ConversionRejection),
}
impl<E> From<E> for SinkRejection<E> {
    fn from(error: E) -> Self {
        Self::Ordinary(error)
    }
}
impl<E> SinkRejection<E> {
    pub(crate) fn map_ordinary<F>(self, f: impl FnOnce(E) -> F) -> SinkRejection<F> {
        match self {
            Self::Ordinary(error) => SinkRejection::Ordinary(f(error)),
            Self::Conversion(error) => SinkRejection::Conversion(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r5d_operational_export_proof_requires_p1_sink_boundaries() {
        // The pre-apply export check uses an operational proof because P1
        // rejects conversions in exports and function sinks. When B2 makes
        // either a legal slot, that check needs its own conversion verdict.
        for (position, kind) in [
            (
                "export declaration",
                ExprKind::<RelativeBindingName>::Declaration,
            ),
            (
                "function sink",
                ExprKind::Function {
                    name: String::new(),
                    signature: Arc::new(FunctionSignature {
                        parameters: Vec::new(),
                        return_type: None,
                        parameter_nodes: Vec::new(),
                        default_nodes: Vec::new(),
                    }),
                },
            ),
        ] {
            assert_eq!(
                PlacementRole::of(&kind),
                PlacementRole::Boundary,
                "{position}: B2 must replace the pre-apply export check's operational proof with a conversion verdict"
            );
        }
    }
}
