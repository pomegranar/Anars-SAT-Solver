//! Simplification applied once, before search.
//!
//! Everything here is standard and everything here is *bounded*. The centrepiece is bounded
//! variable elimination: Davis–Putnam resolution applied only where it does not make the formula
//! bigger. Run without that bound, resolution is a decision procedure that explodes; run with it,
//! it is the preprocessing step that every competitive solver adopted.
//!
//! The pipeline is: normalise, drop duplicate clauses, propagate units, eliminate pure literals,
//! then bounded variable elimination — repeated until it stops paying off or the work budget runs
//! out.

use crate::cnf::Cnf;
use crate::elim::{
    Reconstruction, Working, eliminate_pure_literals, eliminate_var, resolvent_count,
    unit_propagate,
};
use crate::lit::Var;
use std::collections::HashSet;

/// Limits on how much work the preprocessor may do.
#[derive(Copy, Clone, Debug)]
pub struct Limits {
    /// Longest resolvent bounded variable elimination will accept.
    pub max_resolvent_len: usize,
    /// Skip variables with more occurrences than this; they are where the quadratic blowup lives.
    pub max_occurrences: usize,
    /// Rough cap on resolution attempts, so preprocessing cannot dominate the solve.
    pub work_budget: u64,
    /// Number of full simplification rounds.
    pub rounds: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_resolvent_len: 16, max_occurrences: 64, work_budget: 20_000_000, rounds: 4 }
    }
}

/// What preprocessing did.
#[derive(Copy, Clone, Debug, Default)]
pub struct PreprocessStats {
    /// Clauses removed as duplicates.
    pub duplicates_removed: usize,
    /// Variables fixed by unit propagation.
    pub units_fixed: usize,
    /// Variables fixed for being pure.
    pub pure_fixed: usize,
    /// Variables removed by bounded variable elimination.
    pub vars_eliminated: usize,
    /// Clauses in, clauses out.
    pub clauses_before: usize,
    /// Clauses remaining.
    pub clauses_after: usize,
}

/// The simplified formula and how to undo the simplification.
#[derive(Debug)]
pub struct Preprocessed {
    /// The formula the search should run on.
    pub cnf: Cnf,
    /// Set when preprocessing settled the question by itself: `Some(false)` for unsatisfiable,
    /// `Some(true)` when every clause was removed.
    pub verdict: Option<bool>,
    /// Replay trail that turns a model of `cnf` into a model of the input.
    pub reconstruction: Reconstruction,
    /// What was done.
    pub stats: PreprocessStats,
}

/// Simplifies a formula.
///
/// The returned [`Preprocessed::cnf`] keeps the original variable numbering — eliminated
/// variables simply stop occurring — so nothing downstream has to translate indices, and the
/// class of bugs that comes with renumbering does not arise.
#[must_use]
pub fn preprocess(cnf: &Cnf, limits: &Limits) -> Preprocessed {
    let mut stats = PreprocessStats { clauses_before: cnf.num_clauses(), ..Default::default() };
    let (normalised, has_empty) = cnf.normalized();
    let mut trail = Reconstruction::new();

    if has_empty {
        return Preprocessed {
            cnf: normalised,
            verdict: Some(false),
            reconstruction: trail,
            stats,
        };
    }

    let deduped = drop_duplicate_clauses(&normalised, &mut stats);
    let mut w = Working::new(&deduped);
    let mut work = 0_u64;

    for _ in 0..limits.rounds {
        let before_steps = trail.len();

        if !unit_propagate(&mut w, &mut trail) {
            return finish(&w, Some(false), trail, stats);
        }
        stats.units_fixed = trail.len();

        stats.pure_fixed += eliminate_pure_literals(&mut w, &mut trail);
        if w.is_empty() {
            return finish(&w, Some(true), trail, stats);
        }

        let eliminated = bounded_variable_elimination(&mut w, &mut trail, limits, &mut work);
        stats.vars_eliminated += eliminated;

        if w.has_empty_clause() {
            return finish(&w, Some(false), trail, stats);
        }
        if w.is_empty() {
            return finish(&w, Some(true), trail, stats);
        }
        // Nothing changed this round, and nothing will next round either.
        if trail.len() == before_steps {
            break;
        }
        w.compact_occurrences();
    }

    finish(&w, None, trail, stats)
}

fn finish(
    w: &Working,
    verdict: Option<bool>,
    reconstruction: Reconstruction,
    mut stats: PreprocessStats,
) -> Preprocessed {
    let cnf = w.to_cnf();
    stats.clauses_after = cnf.num_clauses();
    // `units_fixed` counted every step, not just units; correct it to the fixes attributable to
    // propagation by subtracting the ones attributed elsewhere.
    stats.units_fixed = stats.units_fixed.saturating_sub(stats.pure_fixed);
    Preprocessed { cnf, verdict, reconstruction, stats }
}

/// Removes clauses that are literally identical.
///
/// The formula arrives sorted from [`Cnf::normalized`], so identity is slice equality.
fn drop_duplicate_clauses(cnf: &Cnf, stats: &mut PreprocessStats) -> Cnf {
    let mut seen: HashSet<&[crate::lit::Lit]> = HashSet::with_capacity(cnf.num_clauses());
    let mut out = Cnf::new(cnf.num_vars());
    for clause in cnf.clauses() {
        if seen.insert(clause) {
            out.add_clause(clause);
        } else {
            stats.duplicates_removed += 1;
        }
    }
    out
}

/// Eliminates variables whose removal does not grow the formula.
///
/// The acceptance test is Eén and Biere's: a variable goes if its non-tautological resolvents
/// number no more than the clauses they replace. Variables are tried cheapest-first, measured by
/// the product of their occurrence counts, because that product bounds the resolution work.
fn bounded_variable_elimination(
    w: &mut Working,
    trail: &mut Reconstruction,
    limits: &Limits,
    work: &mut u64,
) -> usize {
    let mut candidates: Vec<(usize, Var)> = (0..w.num_vars())
        .map(Var::from_index)
        .filter_map(|v| {
            let pos = w.count(v.positive());
            let neg = w.count(v.negative());
            // A variable occurring on only one side is pure, and handled elsewhere.
            if pos == 0 || neg == 0 || pos > limits.max_occurrences || neg > limits.max_occurrences
            {
                None
            } else {
                Some((pos * neg, v))
            }
        })
        .collect();
    candidates.sort_unstable_by_key(|&(cost, v)| (cost, v.index()));

    let mut eliminated = 0;
    for (cost, var) in candidates {
        if *work >= limits.work_budget {
            break;
        }
        *work += cost as u64;

        let pos = w.count(var.positive());
        let neg = w.count(var.negative());
        if pos == 0 || neg == 0 {
            continue;
        }
        // Re-check against the *current* formula: earlier eliminations move the goalposts.
        if pos > limits.max_occurrences || neg > limits.max_occurrences {
            continue;
        }
        if longest_resolvent(w, var) > limits.max_resolvent_len {
            continue;
        }
        if resolvent_count(w, var) > pos + neg {
            continue;
        }
        eliminate_var(w, var, trail);
        eliminated += 1;
    }
    eliminated
}

/// Length of the longest resolvent eliminating `var` would produce.
fn longest_resolvent(w: &Working, var: Var) -> usize {
    let positives = w.occurrences(var.positive());
    let negatives = w.occurrences(var.negative());
    let mut longest = 0;
    for &p in &positives {
        for &n in &negatives {
            let a = w.clause(p).expect("live index");
            let b = w.clause(n).expect("live index");
            if let Some(r) = crate::elim::resolve(a, b, var) {
                longest = longest.max(r.len());
            }
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cnf::Model;

    fn cnf(clauses: &[&[i32]]) -> Cnf {
        let mut f = Cnf::new(0);
        for c in clauses {
            f.add_dimacs_clause(c);
        }
        f
    }

    /// Brute-force satisfiability, for cross-checking.
    fn brute_force(f: &Cnf) -> Option<Model> {
        let n = f.num_vars();
        assert!(n <= 20, "brute force is exponential");
        (0..1_u32 << n)
            .map(|mask| Model::from_values((0..n).map(|i| mask >> i & 1 == 1).collect()))
            .find(|m| f.is_satisfied_by(m))
    }

    #[test]
    fn an_empty_clause_is_caught_immediately() {
        let mut f = cnf(&[&[1]]);
        f.add_clause(&[]);
        let p = preprocess(&f, &Limits::default());
        assert_eq!(p.verdict, Some(false));
    }

    #[test]
    fn duplicate_clauses_are_dropped() {
        let f = cnf(&[&[1, 2], &[2, 1], &[1, 2], &[3, 4]]);
        let p = preprocess(&f, &Limits::default());
        assert_eq!(p.stats.duplicates_removed, 2);
    }

    #[test]
    fn a_trivially_satisfiable_formula_is_solved_outright() {
        // Everything here is pure or unit, so search should never be needed.
        let f = cnf(&[&[1], &[2, 3], &[3, 4]]);
        let p = preprocess(&f, &Limits::default());
        assert_eq!(p.verdict, Some(true));

        let mut m = Model::all_false(f.num_vars());
        p.reconstruction.extend(&mut m);
        assert!(f.is_satisfied_by(&m));
    }

    #[test]
    fn contradictory_units_are_caught() {
        let p = preprocess(&cnf(&[&[1], &[-1], &[2, 3]]), &Limits::default());
        assert_eq!(p.verdict, Some(false));
    }

    #[test]
    fn bounded_elimination_does_not_grow_the_formula() {
        // A chain where each variable links exactly two clauses: elimination is free.
        let f = cnf(&[&[1, 2], &[-2, 3], &[-3, 4], &[-4, 5], &[-5, -1]]);
        let p = preprocess(&f, &Limits::default());
        assert!(
            p.cnf.num_clauses() <= f.num_clauses(),
            "preprocessing grew the formula: {} -> {}",
            f.num_clauses(),
            p.cnf.num_clauses()
        );
    }

    #[test]
    fn elimination_respects_the_occurrence_limit() {
        // One variable in many clauses; with max_occurrences 2 it must be left alone.
        let f = cnf(&[&[1, 2], &[1, 3], &[1, 4], &[-1, 5], &[-1, 6], &[-1, 7]]);
        let limits = Limits { max_occurrences: 2, ..Limits::default() };
        let p = preprocess(&f, &limits);
        // Pure literal elimination may still finish it off; what matters is that BVE did not run
        // away on the high-degree variable.
        assert!(p.stats.vars_eliminated <= 1);
    }

    /// The property that matters: whatever preprocessing does, a model of the residual must
    /// extend to a model of the input.
    #[test]
    fn reconstruction_round_trips_on_random_formulas() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for case in 0..300 {
            let num_vars = 6 + (next() % 5) as usize;
            let num_clauses = 8 + (next() % 20) as usize;
            let mut f = Cnf::new(num_vars);
            for _ in 0..num_clauses {
                let len = 2 + (next() % 2) as usize;
                let mut clause = Vec::new();
                for _ in 0..len {
                    let v = (next() % num_vars as u64) as i32 + 1;
                    let sign = if next() % 2 == 0 { 1 } else { -1 };
                    clause.push(v * sign);
                }
                f.add_dimacs_clause(&clause);
            }

            let p = preprocess(&f, &Limits::default());
            let truth = brute_force(&f);

            match p.verdict {
                Some(false) => assert!(truth.is_none(), "case {case}: claimed UNSAT but is SAT"),
                Some(true) => {
                    let mut m = Model::all_false(f.num_vars());
                    p.reconstruction.extend(&mut m);
                    assert!(f.is_satisfied_by(&m), "case {case}: claimed SAT with a bad model");
                }
                None => {
                    // The residual must have exactly the same satisfiability as the input, and a
                    // model of it must extend to one of the input.
                    let residual_model = brute_force(&p.cnf);
                    assert_eq!(
                        residual_model.is_some(),
                        truth.is_some(),
                        "case {case}: preprocessing changed satisfiability"
                    );
                    if let Some(mut m) = residual_model {
                        p.reconstruction.extend(&mut m);
                        assert!(
                            f.is_satisfied_by(&m),
                            "case {case}: reconstruction produced a non-model"
                        );
                    }
                }
            }
        }
    }
}
