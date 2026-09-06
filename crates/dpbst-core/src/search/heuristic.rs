//! Choosing the next variable to branch on.
//!
//! Every heuristic here scores literals by counting their occurrences in the *active* clauses of
//! the component being solved. That is affordable precisely because component analysis has
//! already walked those clauses; the scan is over a subproblem, not the whole formula.
//!
//! Notably absent is VSIDS, the heuristic that made modern CDCL solvers fast. VSIDS scores
//! variables by how often they appear in *learned* clauses, and this solver does not learn any.
//! The literature's answer for caching solvers is VSADS (Sang, Beame and Kautz), which blends
//! VSIDS with occurrence counting; it is listed as future work in the README rather than
//! pretended at here.

use crate::lit::{Lit, Var};
use crate::search::component::{ComponentRef, ComponentStore};
use crate::search::state::SearchState;

/// Which branching rule to use.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Heuristic {
    /// Jeroslow–Wang: weight each occurrence by `2^-len`, so short clauses dominate.
    #[default]
    JeroslowWang,
    /// Dynamic Largest Individual Sum: pick the single literal occurring most often.
    Dlis,
    /// Dynamic Largest Combined Sum: pick the variable whose two literals occur most often.
    Dlcs,
    /// Maximum Occurrences in clauses of Minimum Size.
    Mom,
    /// Lowest unassigned index, positive first. Deterministic and heuristic-free; used to
    /// isolate data-structure effects in benchmarks.
    Static,
}

impl Heuristic {
    /// Every heuristic, in a stable order for benchmark tables.
    pub const ALL: [Self; 5] =
        [Self::JeroslowWang, Self::Dlis, Self::Dlcs, Self::Mom, Self::Static];

    /// The heuristic's CLI name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::JeroslowWang => "jw",
            Self::Dlis => "dlis",
            Self::Dlcs => "dlcs",
            Self::Mom => "mom",
            Self::Static => "static",
        }
    }
}

impl std::str::FromStr for Heuristic {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|h| h.name() == s)
            .ok_or_else(|| format!("unknown heuristic `{s}`"))
    }
}

impl std::fmt::Display for Heuristic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Weight applied to the product term in the MOM score.
const MOM_PRODUCT_WEIGHT: f64 = 1024.0;

/// Reusable scratch for scoring a component's literals.
#[derive(Debug)]
pub struct DecisionMaker {
    /// Score per literal index; only the entries for the current component are ever non-zero.
    scores: Vec<f64>,
}

impl DecisionMaker {
    /// Allocates scratch for a formula with `num_vars` variables.
    #[must_use]
    pub fn new(num_vars: usize) -> Self {
        Self { scores: vec![0.0; 2 * num_vars] }
    }

    /// Chooses a literal to branch on within `component`.
    ///
    /// Returns `None` only if the component has no unassigned variables, which the search treats
    /// as an already-solved component.
    pub fn pick(
        &mut self,
        state: &SearchState,
        store: &ComponentStore,
        component: ComponentRef,
        heuristic: Heuristic,
    ) -> Option<Lit> {
        let vars = store.vars(component);
        if heuristic == Heuristic::Static {
            return vars
                .iter()
                .copied()
                .find(|&v| state.value_of_index(v as usize).is_none())
                .map(|v| Var::from_index(v as usize).positive());
        }

        let clauses = store.clauses(component);
        // MOM needs the minimum active clause length before it can score anything.
        let min_len = if heuristic == Heuristic::Mom {
            clauses
                .iter()
                .filter(|&&c| state.is_active(c as usize))
                .map(|&c| Self::active_len(state, c as usize))
                .min()
                .unwrap_or(0)
        } else {
            0
        };

        for &c in clauses {
            let c = c as usize;
            if !state.is_active(c) {
                continue;
            }
            let len = Self::active_len(state, c);
            if len == 0 {
                continue;
            }
            let weight = match heuristic {
                // 2^-len: a binary clause counts four times a quaternary one.
                Heuristic::JeroslowWang => (2.0_f64).powi(-(len as i32)),
                Heuristic::Dlis | Heuristic::Dlcs => 1.0,
                Heuristic::Mom => {
                    if len == min_len {
                        1.0
                    } else {
                        continue;
                    }
                }
                Heuristic::Static => unreachable!("handled above"),
            };
            for &l in state.clause(c) {
                if state.value_of_index(l.var().index()).is_none() {
                    self.scores[l.index()] += weight;
                }
            }
        }

        let mut best: Option<(f64, Lit)> = None;
        for &v in vars {
            if state.value_of_index(v as usize).is_some() {
                continue;
            }
            let var = Var::from_index(v as usize);
            let pos = self.scores[var.positive().index()];
            let neg = self.scores[var.negative().index()];

            let rank = match heuristic {
                // DLIS ranks by the best single literal; the others by the variable as a whole.
                Heuristic::Dlis => pos.max(neg),
                // MOM rewards variables that are active on *both* sides, because branching on
                // them is what actually shrinks the minimum-size clauses.
                Heuristic::Mom => pos * neg * MOM_PRODUCT_WEIGHT + pos + neg,
                _ => pos + neg,
            };
            // Branch toward the polarity that satisfies more of what is left; ties go positive
            // so that runs are reproducible.
            let lit = var.lit(pos >= neg);
            if best.is_none_or(|(b, _)| rank > b) {
                best = Some((rank, lit));
            }
        }

        for &v in vars {
            let var = Var::from_index(v as usize);
            self.scores[var.positive().index()] = 0.0;
            self.scores[var.negative().index()] = 0.0;
        }

        best.map(|(_, l)| l)
    }

    /// Unassigned literals remaining in an active clause.
    #[inline]
    fn active_len(state: &SearchState, clause: usize) -> usize {
        state.clause(clause).iter().filter(|l| state.value_of_index(l.var().index()).is_none()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cnf::Cnf;
    use crate::search::component::ComponentAnalyzer;

    fn cnf(clauses: &[&[i32]]) -> Cnf {
        let mut f = Cnf::new(0);
        for c in clauses {
            f.add_dimacs_clause(c);
        }
        f
    }

    fn only_component(f: &Cnf) -> (SearchState, ComponentStore, ComponentRef) {
        let state = SearchState::new(f);
        let mut analyzer = ComponentAnalyzer::new(f.num_vars(), f.num_clauses());
        let mut store = ComponentStore::new();
        let scope: Vec<u32> = (0..f.num_vars() as u32).collect();
        let range = analyzer.analyze(&state, &scope, &mut store, false);
        (state, store, ComponentRef::new(range.start))
    }

    #[test]
    fn names_round_trip() {
        for h in Heuristic::ALL {
            assert_eq!(h.name().parse::<Heuristic>(), Ok(h));
        }
        assert!("nonsense".parse::<Heuristic>().is_err());
    }

    #[test]
    fn static_picks_the_lowest_unassigned_variable() {
        let f = cnf(&[&[3, 4], &[2, 3]]);
        let (state, store, comp) = only_component(&f);
        let mut d = DecisionMaker::new(f.num_vars());
        let lit = d.pick(&state, &store, comp, Heuristic::Static).unwrap();
        assert_eq!(lit, Lit::from_dimacs(2));
    }

    #[test]
    fn jeroslow_wang_prefers_the_variable_in_short_clauses() {
        // Variable 1 appears in one binary clause; variable 4 in three long ones. JW weights the
        // binary clause at 1/4 each and the 4-literal clauses at 1/16, so 1 should win.
        let f = cnf(&[&[1, 2], &[4, 5, 6, 7], &[4, 5, 6, 8], &[4, 5, 6, 9]]);
        let (state, store, comp) = only_component(&f);
        let mut d = DecisionMaker::new(f.num_vars());
        let lit = d.pick(&state, &store, comp, Heuristic::JeroslowWang).unwrap();
        assert!(
            matches!(lit.var().to_dimacs(), 1 | 2),
            "expected a variable from the binary clause, got {lit:?}"
        );
    }

    #[test]
    fn dlis_prefers_the_most_frequent_literal() {
        let f = cnf(&[&[1, 2], &[1, 3], &[1, 4], &[2, 3]]);
        let (state, store, comp) = only_component(&f);
        let mut d = DecisionMaker::new(f.num_vars());
        let lit = d.pick(&state, &store, comp, Heuristic::Dlis).unwrap();
        assert_eq!(lit, Lit::from_dimacs(1), "literal 1 occurs three times");
    }

    #[test]
    fn mom_prefers_a_variable_active_on_both_sides_of_short_clauses() {
        // Clauses of length 2 are the minimum; variable 1 appears in them both positively and
        // negatively, so its product term dominates.
        let f = cnf(&[&[1, 2], &[-1, 3], &[4, 5, 6], &[4, 5, 7]]);
        let (state, store, comp) = only_component(&f);
        let mut d = DecisionMaker::new(f.num_vars());
        let lit = d.pick(&state, &store, comp, Heuristic::Mom).unwrap();
        assert_eq!(lit.var().to_dimacs(), 1);
    }

    #[test]
    fn polarity_follows_the_majority() {
        let f = cnf(&[&[-1, 2], &[-1, 3], &[-1, 4], &[1, 5]]);
        let (state, store, comp) = only_component(&f);
        let mut d = DecisionMaker::new(f.num_vars());
        let lit = d.pick(&state, &store, comp, Heuristic::Dlcs).unwrap();
        assert_eq!(lit, Lit::from_dimacs(-1), "-1 occurs more often than 1");
    }

    #[test]
    fn scores_do_not_leak_between_calls() {
        let f = cnf(&[&[1, 2], &[1, 3], &[1, 4]]);
        let (state, store, comp) = only_component(&f);
        let mut d = DecisionMaker::new(f.num_vars());
        let first = d.pick(&state, &store, comp, Heuristic::Dlis).unwrap();
        let second = d.pick(&state, &store, comp, Heuristic::Dlis).unwrap();
        assert_eq!(first, second, "a repeated call on the same state must be stable");
    }

    #[test]
    fn every_heuristic_returns_a_variable_of_the_component() {
        let f = cnf(&[&[1, -2, 3], &[-1, 2], &[2, -3]]);
        let (state, store, comp) = only_component(&f);
        let mut d = DecisionMaker::new(f.num_vars());
        for h in Heuristic::ALL {
            let lit = d.pick(&state, &store, comp, h).expect("component is non-empty");
            assert!(
                store.vars(comp).contains(&(lit.var().index() as u32)),
                "{h} picked {lit:?}, which is outside the component"
            );
        }
    }
}
