//! Resolution authority is issued by configuration loaders, never by value
//! consumers. Protection is deliberately absent: Secret cannot alter demand.

use crate::value_walk::ExpressionEdge;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResolutionPhase {
    Analysis(AnalysisMode),
    /// Discovery retains values until a later validation consumer demands them.
    Bootstrap,
    /// Validation, planning and live apply demand the same current values.
    Strict,
    /// Candidate failures stay retained across rechecks until hash verification.
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
    backend_demand: Demand,
}

impl ResolutionContext {
    pub(super) const fn issue(phase: ResolutionPhase) -> Self {
        Self {
            phase,
            backend_demand: Demand::Active,
            demand: match phase {
                ResolutionPhase::Bootstrap | ResolutionPhase::SavedRehydration => Demand::Retained,
                ResolutionPhase::Analysis(_) | ResolutionPhase::Strict => Demand::Active,
            },
        }
    }

    /// Module expansion rechecks editor semantics, while discovery still
    /// retains unconsumed values. It does not enter full validation.
    pub(super) fn module_recheck(self) -> Self {
        match self.phase {
            ResolutionPhase::Analysis(_) => Self::issue(ResolutionPhase::Strict),
            ResolutionPhase::Bootstrap
            | ResolutionPhase::SavedRehydration
            | ResolutionPhase::Strict => self,
        }
    }

    /// A later semantic recheck was strict even when its input came from an
    /// editor parse. Only the owning loader can issue that phase transition.
    pub(super) fn strict_recheck(self) -> Self {
        match self.phase {
            ResolutionPhase::Analysis(_) => Self::issue(ResolutionPhase::Strict),
            // Discovery can enter full validation when a later consumer needs it.
            ResolutionPhase::Bootstrap => self.demanded(),
            // Saved candidates retain attempted failures until hash verification.
            ResolutionPhase::SavedRehydration | ResolutionPhase::Strict => self,
        }
    }

    /// Metadata discovery consumes typed declarations, not current backend values.
    pub(super) fn metadata_only(self) -> Self {
        Self {
            backend_demand: Demand::Retained,
            ..self
        }
    }

    pub(crate) fn backend_is_demanded(self) -> bool {
        self.backend_demand == Demand::Active
    }

    pub(super) fn backend(self) -> Self {
        Self {
            demand: self.backend_demand,
            ..self
        }
    }

    fn demanded(self) -> Self {
        Self {
            demand: Demand::Active,
            backend_demand: Demand::Active,
            ..self
        }
    }

    pub(super) fn snapshot(self) -> Self {
        Self {
            phase: ResolutionPhase::Analysis(AnalysisMode::DeferBuiltins),
            ..self
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
