//! Differential testing against exhaustive truth-table evaluation.
//!
//! Every other test in this repository checks that the solver agrees with *itself* under
//! different configurations. That catches a broken bucket policy but not a systematically wrong
//! search: a solver that concludes `UNSATISFIABLE` a little too eagerly would pass all of them.
//!
//! These tests compare against ground truth instead. Formulas are kept small enough to enumerate
//! all `2^n` assignments, and every configuration must match that verdict exactly — plus, for a
//! satisfiable verdict, produce a model that actually satisfies the original formula.

use dpbst_core::cache::BucketKind;
use dpbst_core::cnf::Model;
use dpbst_core::config::{Algorithm, Config};
use dpbst_core::search::heuristic::Heuristic;
use dpbst_core::solver::{Outcome, solve_here};
use dpbst_core::{Cnf, Lit};

/// xorshift64*, so the corpus is reproducible without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Ground truth by enumeration.
fn brute_force(f: &Cnf) -> bool {
    let n = f.num_vars();
    assert!(
        n <= 20,
        "brute force is exponential; {n} variables is too many"
    );
    (0..1_u32 << n).any(|mask| {
        let model = Model::from_values((0..n).map(|i| mask >> i & 1 == 1).collect());
        f.is_satisfied_by(&model)
    })
}

/// A random CNF over `num_vars` variables with `num_clauses` clauses of width `width`.
fn random_cnf(rng: &mut Rng, num_vars: usize, num_clauses: usize, width: usize) -> Cnf {
    let mut f = Cnf::new(num_vars);
    let mut clause: Vec<Lit> = Vec::with_capacity(width);
    for _ in 0..num_clauses {
        clause.clear();
        while clause.len() < width {
            let var = rng.below(num_vars as u64) as i32 + 1;
            let sign = if rng.below(2) == 0 { 1 } else { -1 };
            let lit = Lit::from_dimacs(var * sign);
            // Keep clauses free of repeats and of their own negation.
            if !clause.iter().any(|&l| l.var() == lit.var()) {
                clause.push(lit);
            }
        }
        f.add_clause(&clause);
    }
    f
}

/// Every configuration worth testing. Kept small enough that the product with the corpus stays
/// quick, but covering each independent switch at least once.
/// `(label, config, may_give_up)`. Only Davis-Putnam is allowed to give up: it is a decision
/// procedure bounded by memory, and on a dense random formula it legitimately runs out of room.
fn configurations() -> Vec<(String, Config, bool)> {
    let mut out = Vec::new();
    for bucket in BucketKind::ALL {
        out.push((
            format!("bucket={bucket}"),
            Config {
                bucket,
                ..Config::default()
            },
            false,
        ));
    }
    for heuristic in Heuristic::ALL {
        out.push((
            format!("heuristic={heuristic}"),
            Config {
                heuristic,
                ..Config::default()
            },
            false,
        ));
    }
    out.push((
        "no-cache".into(),
        Config {
            cache: false,
            ..Config::default()
        },
        false,
    ));
    out.push((
        "no-pure-literals".into(),
        Config {
            pure_literals: false,
            ..Config::default()
        },
        false,
    ));
    out.push((
        "no-preprocess".into(),
        Config {
            preprocess: false,
            ..Config::default()
        },
        false,
    ));
    out.push((
        "bare".into(),
        Config {
            cache: false,
            pure_literals: false,
            preprocess: false,
            ..Config::default()
        },
        false,
    ));
    out.push(("plain-dpll".into(), Config::plain_dpll(), false));
    out.push((
        "davis-putnam".into(),
        Config {
            algorithm: Algorithm::DavisPutnam,
            // Resolution on a dense random formula grows fast; cap it so the suite stays quick.
            dp_clause_limit: 20_000,
            ..Config::default()
        },
        true,
    ));
    out.push((
        "tiny-cache".into(),
        // A budget this small forces eviction sweeps mid-search, so the sweep path is exercised
        // against ground truth rather than only in isolation.
        Config {
            cache_budget_bytes: 32 * 1024,
            ..Config::default()
        },
        false,
    ));
    out.push((
        "load=64".into(),
        Config {
            target_load: 64,
            ..Config::default()
        },
        false,
    ));
    out
}

/// Checks one formula against ground truth under every configuration.
fn check(f: &Cnf, label: &str) {
    let truth = brute_force(f);
    for (name, config, may_give_up) in configurations() {
        let result = solve_here(f, &config);
        match &result.outcome {
            Outcome::Sat(model) => {
                assert!(
                    truth,
                    "{label} [{name}]: said SAT, ground truth is UNSAT\n{f}"
                );
                assert!(
                    f.is_satisfied_by(model),
                    "{label} [{name}]: said SAT with a model that fails the formula\n{f}"
                );
            }
            Outcome::Unsat => {
                assert!(
                    !truth,
                    "{label} [{name}]: said UNSAT, ground truth is SAT\n{f}"
                );
            }
            Outcome::Unknown(reason) => {
                assert!(
                    may_give_up,
                    "{label} [{name}]: gave up ({reason}) on a {}-variable formula",
                    f.num_vars()
                );
            }
        }
    }
}

#[test]
fn random_3sat_matches_brute_force() {
    let mut rng = Rng(0x243f_6a88_85a3_08d3);
    for case in 0..400 {
        let num_vars = 4 + (rng.below(8) as usize);
        // Straddle the phase transition so both verdicts show up in quantity.
        let num_clauses = (num_vars as f64 * (2.0 + rng.below(500) as f64 / 100.0)) as usize;
        let f = random_cnf(&mut rng, num_vars, num_clauses.max(1), 3);
        check(&f, &format!("3sat case {case}"));
    }
}

#[test]
fn random_mixed_width_matches_brute_force() {
    let mut rng = Rng(0x1357_9bdf_0246_8ace);
    for case in 0..300 {
        let num_vars = 3 + (rng.below(9) as usize);
        let num_clauses = 1 + (rng.below(30) as usize);
        let width = 1 + (rng.below(3) as usize).min(num_vars - 1);
        let f = random_cnf(&mut rng, num_vars, num_clauses, width.max(1));
        check(&f, &format!("mixed case {case}"));
    }
}

/// Formulas built as several disjoint blocks: the case decomposition and the memo exist for.
#[test]
fn decomposable_formulas_match_brute_force() {
    let mut rng = Rng(0x0f0f_0f0f_dead_beef);
    for case in 0..200 {
        let blocks = 2 + (rng.below(3) as usize);
        let per_block = 3 + (rng.below(2) as usize);
        let mut f = Cnf::new(blocks * per_block);
        for block in 0..blocks {
            let base = (block * per_block) as i32;
            let clauses = 2 + rng.below(8);
            for _ in 0..clauses {
                let mut clause = Vec::new();
                while clause.len() < 3 {
                    let var = base + rng.below(per_block as u64) as i32 + 1;
                    let sign = if rng.below(2) == 0 { 1 } else { -1 };
                    let lit = Lit::from_dimacs(var * sign);
                    if !clause.iter().any(|&l: &Lit| l.var() == lit.var()) {
                        clause.push(lit);
                    }
                }
                f.add_clause(&clause);
            }
        }
        check(&f, &format!("blocks case {case}"));
    }
}

/// Horn formulas: unit propagation alone decides these, so any discrepancy points at the
/// propagation counters rather than the search.
#[test]
fn horn_formulas_match_brute_force() {
    let mut rng = Rng(0xcafe_f00d_1234_5678);
    for case in 0..200 {
        let num_vars = 4 + (rng.below(8) as usize);
        let num_clauses = 2 + (rng.below(25) as usize);
        let mut f = Cnf::new(num_vars);
        for _ in 0..num_clauses {
            let width = 1 + rng.below(3) as usize;
            let mut clause = Vec::new();
            // At most one positive literal makes it Horn.
            for i in 0..width {
                let var = rng.below(num_vars as u64) as i32 + 1;
                let lit = Lit::from_dimacs(if i == 0 { var } else { -var });
                if !clause.iter().any(|&l: &Lit| l.var() == lit.var()) {
                    clause.push(lit);
                }
            }
            f.add_clause(&clause);
        }
        check(&f, &format!("horn case {case}"));
    }
}

/// Pigeonhole: unsatisfiable for every `n`, and the classic hard case for resolution.
#[test]
fn pigeonhole_is_unsatisfiable() {
    for holes in 1..=4 {
        let pigeons = holes + 1;
        let mut f = Cnf::new(pigeons * holes);
        let var = |p: usize, h: usize| (p * holes + h) as i32 + 1;

        // Every pigeon is in some hole.
        for p in 0..pigeons {
            f.add_dimacs_clause(&(0..holes).map(|h| var(p, h)).collect::<Vec<_>>());
        }
        // No hole holds two pigeons.
        for h in 0..holes {
            for p in 0..pigeons {
                for q in p + 1..pigeons {
                    f.add_dimacs_clause(&[-var(p, h), -var(q, h)]);
                }
            }
        }
        check(&f, &format!("pigeonhole {pigeons} into {holes}"));
    }
}
