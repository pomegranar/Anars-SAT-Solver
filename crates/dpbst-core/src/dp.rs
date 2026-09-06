//! The original Davis–Putnam procedure.
//!
//! Published in 1960, two years before the backtracking search that replaced it. It decides
//! satisfiability without any search at all: propagate units, remove pure literals, then pick a
//! variable and *eliminate* it by replacing every clause containing it with all resolvents of its
//! positive and negative occurrences. Repeat. No clauses left means satisfiable; the empty clause
//! means unsatisfiable.
//!
//! It is here for three reasons. It is the other thing "DP" names, so a project called DP-BST
//! ought to ship it. It shares all of its machinery with the bounded variable elimination in
//! [`crate::preprocess`], so it costs almost nothing to provide. And running it next to the DPLL
//! search makes concrete why the 1962 paper won: memory. Eliminating a variable of degree
//! `p` and `n` can produce `p * n` clauses, and that compounds.
//!
//! Elimination order is cheapest-first — minimising `|C_v| * |C_-v|` — which is the standard
//! heuristic and delays the blowup as long as anything can.

use crate::cnf::{Cnf, Model};
use crate::elim::{
    Reconstruction, Working, eliminate_pure_literals, eliminate_var, unit_propagate,
};
use crate::lit::Var;

/// How a Davis–Putnam run ended.
#[derive(Debug)]
pub enum DpOutcome {
    /// Satisfiable, with a model recovered from the elimination trail.
    Sat(Model),
    /// Unsatisfiable.
    Unsat,
    /// The clause limit was hit; the procedure ran out of room, not out of work.
    OutOfResources {
        /// Clauses present when the limit was hit.
        clauses: usize,
        /// Variables eliminated before that.
        eliminated: usize,
    },
}

/// Counters for a Davis–Putnam run.
#[derive(Copy, Clone, Debug, Default)]
pub struct DpStats {
    /// Variables removed by resolution.
    pub eliminated: usize,
    /// Resolvents generated.
    pub resolvents: u64,
    /// Largest clause count reached, the number that decides whether this finishes.
    pub peak_clauses: usize,
}

/// Runs Davis–Putnam to completion, or until `clause_limit` clauses exist.
#[must_use]
pub fn solve(cnf: &Cnf, clause_limit: usize) -> (DpOutcome, DpStats) {
    let (normalised, has_empty) = cnf.normalized();
    let mut stats = DpStats::default();
    let mut trail = Reconstruction::new();

    if has_empty {
        return (DpOutcome::Unsat, stats);
    }

    let mut w = Working::new(&normalised);
    stats.peak_clauses = w.live_clauses();

    loop {
        if !unit_propagate(&mut w, &mut trail) {
            return (DpOutcome::Unsat, stats);
        }
        eliminate_pure_literals(&mut w, &mut trail);

        if w.has_empty_clause() {
            return (DpOutcome::Unsat, stats);
        }
        if w.is_empty() {
            let mut model = Model::all_false(cnf.num_vars());
            trail.extend(&mut model);
            debug_assert!(
                cnf.is_satisfied_by(&model),
                "DP produced a model that fails the input"
            );
            return (DpOutcome::Sat(model), stats);
        }

        // Cheapest variable first: the product of the occurrence counts bounds the resolvents.
        let Some(var) = cheapest_variable(&w) else {
            // Every remaining variable occurs on one side only, so the pure literal rule should
            // have cleared the formula. Reaching here means nothing is left to do.
            let mut model = Model::all_false(cnf.num_vars());
            trail.extend(&mut model);
            return (DpOutcome::Sat(model), stats);
        };

        stats.resolvents += eliminate_var(&mut w, var, &mut trail) as u64;
        stats.eliminated += 1;
        stats.peak_clauses = stats.peak_clauses.max(w.live_clauses());

        if w.live_clauses() > clause_limit {
            return (
                DpOutcome::OutOfResources {
                    clauses: w.live_clauses(),
                    eliminated: stats.eliminated,
                },
                stats,
            );
        }
        // Occurrence lists accumulate tombstones fast under resolution, but compaction is not
        // cheap either; only pay for it once most entries are dead.
        if w.tombstone_ratio() > 0.5 {
            w.compact_occurrences();
        }
    }
}

/// The variable whose elimination looks cheapest.
///
/// Ranked by `|C_v| * |C_-v|`, the number of resolution attempts, rather than by the exact
/// resolvent count. Counting exactly means performing every resolution for every candidate
/// variable on every iteration, which costs more than the elimination it is choosing.
fn cheapest_variable(w: &Working) -> Option<Var> {
    let counts = w.literal_counts();
    (0..w.num_vars())
        .map(Var::from_index)
        .filter_map(|v| {
            let pos = u64::from(counts[v.positive().index()]);
            let neg = u64::from(counts[v.negative().index()]);
            if pos > 0 && neg > 0 {
                Some((pos * neg, v))
            } else {
                None
            }
        })
        .min()
        .map(|(_, v)| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cnf(clauses: &[&[i32]]) -> Cnf {
        let mut f = Cnf::new(0);
        for c in clauses {
            f.add_dimacs_clause(c);
        }
        f
    }

    fn verdict(f: &Cnf) -> Option<bool> {
        match solve(f, 1_000_000).0 {
            DpOutcome::Sat(m) => {
                assert!(f.is_satisfied_by(&m), "model does not satisfy the formula");
                Some(true)
            }
            DpOutcome::Unsat => Some(false),
            DpOutcome::OutOfResources { .. } => None,
        }
    }

    #[test]
    fn empty_formula_is_satisfiable() {
        assert_eq!(verdict(&Cnf::new(3)), Some(true));
    }

    #[test]
    fn a_direct_contradiction_is_unsatisfiable() {
        assert_eq!(verdict(&cnf(&[&[1], &[-1]])), Some(false));
    }

    #[test]
    fn all_four_two_variable_clauses_are_unsatisfiable() {
        assert_eq!(
            verdict(&cnf(&[&[1, 2], &[1, -2], &[-1, 2], &[-1, -2]])),
            Some(false)
        );
    }

    #[test]
    fn a_satisfiable_instance_yields_a_checked_model() {
        assert_eq!(
            verdict(&cnf(&[&[1, 2, 3], &[-1, 2], &[-2, 3], &[-3, 1]])),
            Some(true)
        );
    }

    #[test]
    fn resolution_finds_the_pigeonhole_contradiction() {
        // Three pigeons, two holes.
        let f = cnf(&[
            &[1, 2],
            &[3, 4],
            &[5, 6],
            &[-1, -3],
            &[-1, -5],
            &[-3, -5],
            &[-2, -4],
            &[-2, -6],
            &[-4, -6],
        ]);
        assert_eq!(verdict(&f), Some(false));
    }

    #[test]
    fn the_clause_limit_stops_a_blowup() {
        // A formula designed to resolve badly, with a limit far below what it needs.
        let mut f = Cnf::new(12);
        for i in 1..=6_i32 {
            for j in 7..=12_i32 {
                f.add_dimacs_clause(&[i, j]);
                f.add_dimacs_clause(&[-i, -j]);
            }
        }
        assert!(matches!(solve(&f, 8).0, DpOutcome::OutOfResources { .. }));
    }

    #[test]
    fn peak_clause_count_is_reported() {
        let f = cnf(&[&[1, 2], &[-1, 3], &[-2, 3], &[-3, 4], &[-4, 1]]);
        let (_, stats) = solve(&f, 1_000_000);
        assert!(stats.peak_clauses >= f.num_clauses() - 1);
    }
}
