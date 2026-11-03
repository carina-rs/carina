use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::{ProviderSchemaValidationError, RefinedScalarKind, TypeIdentity};

/// A provider's directional assertion that `source` is assignable to `sink`.
/// Endpoint membership and scalar kinds are checked by the aggregate witness.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AssignabilityEdge {
    source: TypeIdentity,
    sink: TypeIdentity,
}

impl AssignabilityEdge {
    /// Self assignment is implicit and must not be declared as an edge.
    pub fn new(
        source: TypeIdentity,
        sink: TypeIdentity,
    ) -> Result<Self, ProviderSchemaValidationError> {
        if source == sink {
            return Err(ProviderSchemaValidationError::SelfAssignabilityEdge { identity: source });
        }
        Ok(Self { source, sink })
    }

    /// Borrow every declaration field without exposing mutable endpoints.
    pub fn parts(&self) -> (&TypeIdentity, &TypeIdentity) {
        let Self { source, sink } = self;
        (source, sink)
    }
}

/// Transitive closure of one admitted provider's explicit edges.
#[derive(Debug, Clone, Default)]
pub(crate) struct AssignabilityClosure {
    sinks: BTreeMap<TypeIdentity, BTreeSet<TypeIdentity>>,
}

impl AssignabilityClosure {
    pub(super) fn validate(
        edges: &[AssignabilityEdge],
        scalar_kinds: &BTreeMap<TypeIdentity, RefinedScalarKind>,
    ) -> Result<Self, ProviderSchemaValidationError> {
        // Validate in identity order while preserving declaration order on the
        // witness and wire. Duplicate diagnostics do not depend on input order.
        let mut ordered: Vec<_> = edges.iter().collect();
        ordered.sort();
        for pair in ordered.windows(2) {
            if pair[0] == pair[1] {
                return Err(ProviderSchemaValidationError::DuplicateAssignabilityEdge {
                    source: Box::new(pair[0].source.clone()),
                    sink: Box::new(pair[0].sink.clone()),
                });
            }
        }

        let mut adjacency = BTreeMap::<TypeIdentity, BTreeSet<TypeIdentity>>::new();
        for edge in ordered {
            let AssignabilityEdge { source, sink } = edge;
            let kind = |identity: &TypeIdentity| {
                scalar_kinds.get(identity).copied().ok_or_else(|| {
                    ProviderSchemaValidationError::DanglingAssignabilityEndpoint {
                        identity: identity.clone(),
                    }
                })
            };
            let source_kind = kind(source)?;
            let sink_kind = kind(sink)?;
            if source_kind != sink_kind {
                return Err(
                    ProviderSchemaValidationError::AssignabilityScalarKindMismatch {
                        source: Box::new(source.clone()),
                        sink: Box::new(sink.clone()),
                        source_kind,
                        sink_kind,
                    },
                );
            }
            adjacency
                .entry(source.clone())
                .or_default()
                .insert(sink.clone());
        }

        let mut closure = Self::default();
        for source in adjacency.keys() {
            let mut reached = BTreeSet::new();
            let mut parents = BTreeMap::<&TypeIdentity, &TypeIdentity>::new();
            let mut pending = VecDeque::from([source]);
            while let Some(current) = pending.pop_front() {
                let Some(sinks) = adjacency.get(current) else {
                    continue;
                };
                for sink in sinks {
                    if sink == source {
                        let mut cycle = Vec::new();
                        let mut member = current;
                        while member != source {
                            cycle.push(member.clone());
                            member = parents[member];
                        }
                        cycle.push(source.clone());
                        cycle.reverse();
                        cycle.push(source.clone());
                        return Err(ProviderSchemaValidationError::AssignabilityCycle { cycle });
                    }
                    if reached.insert(sink.clone()) {
                        parents.insert(sink, current);
                        pending.push_back(sink);
                    }
                }
            }
            closure.sinks.insert(source.clone(), reached);
        }
        Ok(closure)
    }

    pub(crate) fn reachable(&self, source: &TypeIdentity, sink: &TypeIdentity) -> bool {
        self.sinks
            .get(source)
            .is_some_and(|sinks| sinks.contains(sink))
    }
}
