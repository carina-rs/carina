//! Resolution authority is issued by configuration loaders, never by value
//! consumers. Protection is deliberately absent: Secret cannot alter demand.

use crate::value_walk::ExpressionEdge;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResolutionPhase {
    Analysis(AnalysisMode),
    Bootstrap,
    ValidateStrict,
    Plan,
    Apply,
    SavedRehydration,
}

/// These analysis passes have deliberately different failure contracts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AnalysisMode {
    DeferBuiltins,
    Diagnostics,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Demand {
    Active,
    Retained,
}

/// A sealed phase/demand pair. Only the owning loader can issue a root;
/// consumers may lower its demand, but cannot change its phase.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResolutionContext {
    phase: ResolutionPhase,
    demand: Demand,
}

impl ResolutionContext {
    pub(super) const fn issue(phase: ResolutionPhase) -> Self {
        Self {
            phase,
            demand: Demand::Active,
        }
    }

    /// A later semantic recheck was strict even when its input came from an
    /// editor parse. Only the owning loader can issue that phase transition.
    pub(super) fn strict_recheck(self) -> Self {
        if matches!(self.phase, ResolutionPhase::Analysis(_)) {
            Self::issue(ResolutionPhase::ValidateStrict)
        } else {
            self
        }
    }

    /// Bootstrap defers resource attribute values only. Backend settings,
    /// directives and other declarations still have their strict contract.
    /// The loader owns this choice of phase for a declaration root.
    pub(super) fn declarations(self) -> Self {
        if self.phase == ResolutionPhase::Bootstrap {
            Self::issue(ResolutionPhase::ValidateStrict)
        } else {
            self
        }
    }

    pub(crate) fn retained(self) -> Self {
        Self {
            demand: Demand::Retained,
            ..self
        }
    }

    pub(crate) fn is_diagnostics(self) -> bool {
        matches!(
            self.phase,
            ResolutionPhase::Analysis(AnalysisMode::Diagnostics)
        )
    }

    pub(crate) fn requires_available(self) -> bool {
        self.demand == Demand::Active
            && !matches!(
                self.phase,
                ResolutionPhase::Analysis(AnalysisMode::DeferBuiltins)
                    | ResolutionPhase::Bootstrap
                    | ResolutionPhase::SavedRehydration
            )
    }

    pub(crate) fn retains_failed_calls(self) -> bool {
        self.demand == Demand::Retained
            || matches!(
                self.phase,
                ResolutionPhase::Analysis(AnalysisMode::Diagnostics)
            )
    }

    /// Expression edges carry demand independently of protection ancestry.
    pub(crate) fn at_expression_edges(self, edges: &[ExpressionEdge]) -> Self {
        if edges
            .iter()
            .any(|edge| matches!(edge, ExpressionEdge::CoalesceDefault))
        {
            self.retained()
        } else {
            self
        }
    }
}
