//! The public entry point: run a configuration against a formula and get a verdict.

use crate::cache::{Avl, BucketKind, BucketPolicy, Chain, Splay, Unbalanced};
use crate::cnf::{Cnf, Model};
use crate::config::{Algorithm, Config};
use crate::dp::{self, DpOutcome, DpStats};
use crate::preprocess::{self, Limits, PreprocessStats};
use crate::search::{Answer, SearchCounters, Searcher};
use std::time::{Duration, Instant};

/// Stack size for the search thread.
///
/// The search recurses once per decision, so depth is bounded by the number of variables. The
/// default 8 MiB main-thread stack is enough for the benchmark suites but not for a large
/// structured instance, and running off the end of a stack is an abort, not an error. Solving on
/// a thread with room to spare is cheaper than restructuring the recursion into an explicit
/// stack, and keeps the search readable.
const SEARCH_STACK_BYTES: usize = 256 * 1024 * 1024;

/// Why a solve gave up.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AbortReason {
    /// The wall-clock limit expired.
    Timeout,
    /// The search-node limit was reached.
    NodeLimit,
    /// Davis–Putnam exceeded its clause limit.
    Resources,
}

impl std::fmt::Display for AbortReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Timeout => "timeout",
            Self::NodeLimit => "node limit",
            Self::Resources => "resource limit",
        })
    }
}

/// The verdict.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Satisfiable, with a total assignment over the input's variables.
    Sat(Model),
    /// Unsatisfiable.
    Unsat,
    /// No answer within the configured limits.
    Unknown(AbortReason),
}

impl Outcome {
    /// The DIMACS status line for this outcome.
    #[must_use]
    pub const fn status_line(&self) -> &'static str {
        match self {
            Self::Sat(_) => "s SATISFIABLE",
            Self::Unsat => "s UNSATISFIABLE",
            Self::Unknown(_) => "s UNKNOWN",
        }
    }

    /// The conventional SAT competition exit code: 10 satisfiable, 20 unsatisfiable, 0 unknown.
    ///
    /// Matching this is what lets `dpbst` be dropped into the same harnesses as `MiniSat`, `CaDiCaL`
    /// and Kissat, and compared against them without special-casing.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Sat(_) => 10,
            Self::Unsat => 20,
            Self::Unknown(_) => 0,
        }
    }
}

/// Everything measured about one solve.
#[derive(Clone, Debug)]
pub struct SolveStats {
    /// Algorithm used.
    pub algorithm: Algorithm,
    /// Bucket policy used, when the memo was active.
    pub bucket: Option<BucketKind>,
    /// Variables in the input.
    pub input_vars: usize,
    /// Clauses in the input.
    pub input_clauses: usize,
    /// Preprocessing counters, when preprocessing ran.
    pub preprocess: Option<PreprocessStats>,
    /// Search counters, when the search ran.
    pub search: Option<SearchCounters>,
    /// Davis–Putnam counters, for that algorithm.
    pub dp: Option<DpStats>,
    /// Total wall-clock time.
    pub elapsed: Duration,
}

/// A verdict plus its measurements.
#[derive(Clone, Debug)]
pub struct SolveResult {
    /// The verdict.
    pub outcome: Outcome,
    /// What it cost.
    pub stats: SolveStats,
}

impl SolveResult {
    /// Re-checks a satisfiable verdict against `cnf`.
    ///
    /// Returns `Ok(())` for unsatisfiable and unknown verdicts, since there is nothing to check.
    /// The solver calls this itself in debug builds; the CLI exposes it as `--verify`.
    ///
    /// # Errors
    /// Returns the index of a clause the model fails to satisfy.
    pub fn verify(&self, cnf: &Cnf) -> Result<(), usize> {
        match &self.outcome {
            Outcome::Sat(model) => cnf.first_falsified_clause(model).map_or(Ok(()), Err),
            _ => Ok(()),
        }
    }
}

/// Solves `cnf` under `config`, on a thread with a stack deep enough for the recursion.
///
/// # Panics
/// Panics if the search thread cannot be spawned, or if it panics.
#[must_use]
pub fn solve(cnf: &Cnf, config: &Config) -> SolveResult {
    let cnf = cnf.clone();
    let config = config.clone();
    std::thread::Builder::new()
        .name("dpbst-search".into())
        .stack_size(SEARCH_STACK_BYTES)
        .spawn(move || solve_here(&cnf, &config))
        .expect("failed to spawn the search thread")
        .join()
        .expect("the search thread panicked")
}

/// Solves `cnf` on the calling thread.
///
/// Use this only when the caller already guarantees a deep stack; otherwise prefer [`solve`].
#[must_use]
pub fn solve_here(cnf: &Cnf, config: &Config) -> SolveResult {
    let started = Instant::now();
    let deadline = config.timeout.map(|t| started + t);
    let mut stats = SolveStats {
        algorithm: config.algorithm,
        bucket: (config.algorithm != Algorithm::DavisPutnam && config.cache)
            .then_some(config.bucket),
        input_vars: cnf.num_vars(),
        input_clauses: cnf.num_clauses(),
        preprocess: None,
        search: None,
        dp: None,
        elapsed: Duration::ZERO,
    };

    if config.algorithm == Algorithm::DavisPutnam {
        let (outcome, dp_stats) = dp::solve(cnf, config.dp_clause_limit);
        stats.dp = Some(dp_stats);
        stats.elapsed = started.elapsed();
        let outcome = match outcome {
            DpOutcome::Sat(model) => Outcome::Sat(model),
            DpOutcome::Unsat => Outcome::Unsat,
            DpOutcome::OutOfResources { .. } => Outcome::Unknown(AbortReason::Resources),
        };
        return finished(outcome, stats, cnf);
    }

    // Preprocess, unless asked not to. The residual keeps the original variable numbering, so the
    // reconstruction trail is the only thing standing between its model and the input's.
    let (working, reconstruction) = if config.preprocess {
        let p = preprocess::preprocess(cnf, &Limits::default());
        stats.preprocess = Some(p.stats);
        match p.verdict {
            Some(false) => {
                stats.elapsed = started.elapsed();
                return finished(Outcome::Unsat, stats, cnf);
            }
            Some(true) => {
                let mut model = Model::all_false(cnf.num_vars());
                p.reconstruction.extend(&mut model);
                stats.elapsed = started.elapsed();
                return finished(Outcome::Sat(model), stats, cnf);
            }
            None => (p.cnf, p.reconstruction),
        }
    } else {
        (cnf.normalized().0, crate::elim::Reconstruction::new())
    };

    let (answer, model, counters) = match config.bucket {
        BucketKind::Avl => run::<Avl>(&working, config, deadline),
        BucketKind::Unbalanced => run::<Unbalanced>(&working, config, deadline),
        BucketKind::Splay => run::<Splay>(&working, config, deadline),
        BucketKind::Chain => run::<Chain>(&working, config, deadline),
    };
    stats.search = Some(counters);
    stats.elapsed = started.elapsed();

    let outcome = match answer {
        Answer::Unsat => Outcome::Unsat,
        // Every backjump is absorbed by the frame that owns its target level, and the root owns
        // level zero, so one can never escape the search.
        Answer::Backjump(level) => unreachable!("backjump to level {level} escaped the search"),
        Answer::Aborted => Outcome::Unknown(if config.timeout.is_some() {
            AbortReason::Timeout
        } else {
            AbortReason::NodeLimit
        }),
        Answer::Sat => {
            let mut full = Model::all_false(cnf.num_vars());
            for (var, &value) in model.iter().enumerate() {
                full.set(crate::lit::Var::from_index(var), value);
            }
            reconstruction.extend(&mut full);
            Outcome::Sat(full)
        }
    };
    finished(outcome, stats, cnf)
}

/// Runs the search with a concrete bucket policy, so every call is statically dispatched.
fn run<P: BucketPolicy>(
    cnf: &Cnf,
    config: &Config,
    deadline: Option<Instant>,
) -> (Answer, Vec<bool>, SearchCounters) {
    let mut searcher = Searcher::<P>::new(cnf, config, deadline);
    let answer = searcher.run();
    (answer, searcher.model().to_vec(), searcher.counters())
}

/// Final assembly, with a debug-build audit of any satisfiable verdict.
fn finished(outcome: Outcome, stats: SolveStats, original: &Cnf) -> SolveResult {
    let result = SolveResult { outcome, stats };
    debug_assert!(
        result.verify(original).is_ok(),
        "the solver reported SATISFIABLE with a model that fails the input formula"
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::heuristic::Heuristic;

    fn cnf(clauses: &[&[i32]]) -> Cnf {
        let mut f = Cnf::new(0);
        for c in clauses {
            f.add_dimacs_clause(c);
        }
        f
    }

    fn verdict(f: &Cnf, config: &Config) -> Option<bool> {
        let r = solve_here(f, config);
        r.verify(f).expect("model must satisfy the formula");
        match r.outcome {
            Outcome::Sat(_) => Some(true),
            Outcome::Unsat => Some(false),
            Outcome::Unknown(_) => None,
        }
    }

    #[test]
    fn trivial_cases() {
        let c = Config::default();
        assert_eq!(verdict(&Cnf::new(0), &c), Some(true));
        assert_eq!(verdict(&Cnf::new(5), &c), Some(true));
        assert_eq!(verdict(&cnf(&[&[1], &[-1]]), &c), Some(false));
        assert_eq!(
            verdict(&cnf(&[&[1, 2], &[1, -2], &[-1, 2], &[-1, -2]]), &c),
            Some(false)
        );
    }

    #[test]
    fn an_empty_clause_is_unsatisfiable() {
        let mut f = cnf(&[&[1, 2]]);
        f.add_clause(&[]);
        assert_eq!(verdict(&f, &Config::default()), Some(false));
    }

    #[test]
    fn exit_codes_follow_the_competition_convention() {
        assert_eq!(Outcome::Unsat.exit_code(), 20);
        assert_eq!(Outcome::Sat(Model::all_false(1)).exit_code(), 10);
        assert_eq!(Outcome::Unknown(AbortReason::Timeout).exit_code(), 0);
    }

    /// Every configuration must agree, on both the answer and the validity of the model. This is
    /// the test that catches a bucket policy or an ablation quietly changing the semantics.
    #[test]
    fn all_configurations_agree() {
        let instances = [
            cnf(&[&[1, 2, 3], &[-1, 2], &[-2, 3], &[-3, 1], &[-1, -2, -3]]),
            cnf(&[&[1, 2], &[1, -2], &[-1, 2], &[-1, -2]]),
            cnf(&[&[1, 2], &[3, 4], &[-1, -2], &[-3, -4]]),
            cnf(&[&[1], &[-1, 2], &[-2, 3], &[-3, 4], &[-4]]),
        ];

        for (i, f) in instances.iter().enumerate() {
            let baseline = verdict(f, &Config::default());
            for bucket in BucketKind::ALL {
                for heuristic in Heuristic::ALL {
                    for cache in [true, false] {
                        for learn in [true, false] {
                            for pure_literals in [true, false] {
                                for preprocess in [true, false] {
                                    let c = Config {
                                        bucket,
                                        heuristic,
                                        cache,
                                        learn,
                                        pure_literals,
                                        preprocess,
                                        ..Config::default()
                                    };
                                    assert_eq!(
                                        verdict(f, &c),
                                        baseline,
                                        "instance {i} disagreed with bucket={bucket} \
                                         heuristic={heuristic} cache={cache} learn={learn} \
                                         pure={pure_literals} pre={preprocess}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            // Plain DPLL and Davis-Putnam must land on the same answer too.
            assert_eq!(
                verdict(f, &Config::plain_dpll()),
                baseline,
                "instance {i}: plain DPLL"
            );
            let dp = Config {
                algorithm: Algorithm::DavisPutnam,
                ..Config::default()
            };
            assert_eq!(verdict(f, &dp), baseline, "instance {i}: Davis-Putnam");
        }
    }

    /// Four independent "exactly one of three" blocks. Nothing links them, so decomposition
    /// should find them as separate components rather than searching their product.
    #[test]
    fn independent_blocks_are_decomposed() {
        let mut f = Cnf::new(0);
        for block in 0..4_i32 {
            let (x, y, z) = (block * 3 + 1, block * 3 + 2, block * 3 + 3);
            f.add_dimacs_clause(&[x, y, z]);
            f.add_dimacs_clause(&[-x, -y]);
            f.add_dimacs_clause(&[-x, -z]);
            f.add_dimacs_clause(&[-y, -z]);
        }
        let config = Config {
            preprocess: false,
            ..Config::default()
        };
        let r = solve_here(&f, &config);
        assert!(matches!(r.outcome, Outcome::Sat(_)));
        r.verify(&f).expect("model must satisfy");

        let search = r.stats.search.expect("search ran");
        assert!(
            search.components >= 4,
            "expected at least one component per block, got {}",
            search.components
        );
    }

    /// The memo only pays when the *same* subproblem is reached twice by different routes. Here
    /// an unsatisfiable block over {u, v, w} hangs off the clause (a or b or u): setting `a`, or
    /// clearing `a` and setting `b`, both leave that block as the identical residual component.
    /// The second visit must be answered from the memo rather than re-searched.
    #[test]
    fn the_memo_hits_when_a_subproblem_recurs() {
        let (a, b, u, v, w) = (1, 2, 3, 4, 5);
        let mut f = Cnf::new(0);
        f.add_dimacs_clause(&[a, b, u]);
        // Every assignment of u, v, w is excluded, so the block is unsatisfiable.
        for mask in 0..8_i32 {
            f.add_dimacs_clause(&[
                if mask & 1 == 0 { u } else { -u },
                if mask & 2 == 0 { v } else { -v },
                if mask & 4 == 0 { w } else { -w },
            ]);
        }

        // Pure literals would set `a` outright and the block would only ever be seen once, and
        // learning would settle the block from a derived clause rather than revisit it. Both are
        // switched off so that what is measured is the memo.
        let config = Config {
            preprocess: false,
            pure_literals: false,
            learn: false,
            heuristic: Heuristic::Static,
            ..Config::default()
        };
        let with_memo = solve_here(&f, &config);
        assert!(matches!(with_memo.outcome, Outcome::Unsat));

        let stats = with_memo.stats.search.expect("search ran");
        assert!(
            stats.cache.hits > 0,
            "expected the repeated block to be answered from the memo;              lookups={} hits={}",
            stats.cache.lookups,
            stats.cache.hits
        );

        // And with the memo off, the same instance must take strictly more search nodes.
        let without = solve_here(
            &f,
            &Config {
                cache: false,
                ..config
            },
        );
        let plain = without.stats.search.expect("search ran");
        assert!(
            plain.nodes > stats.nodes,
            "memo should have saved work: {} nodes with, {} without",
            stats.nodes,
            plain.nodes
        );
    }

    /// The point of learning: a conflict analysed once must not have to be rediscovered. On an
    /// instance whose contradiction hangs off a wide clause, the difference is not marginal.
    #[test]
    fn learning_cuts_the_search_down() {
        // Every literal of the wide clause implies the same unsatisfiable block, so without
        // learning each of its branches rediscovers the same contradiction from scratch.
        let mut f = Cnf::new(0);
        let width = 12_i32;
        let wide: Vec<i32> = (1..=width).collect();
        f.add_dimacs_clause(&wide);
        for i in 1..=width {
            f.add_dimacs_clause(&[-i, width + 1]);
        }
        let (u, v, w) = (width + 2, width + 3, width + 4);
        for mask in 0..8_i32 {
            f.add_dimacs_clause(&[
                -(width + 1),
                if mask & 1 == 0 { u } else { -u },
                if mask & 2 == 0 { v } else { -v },
                if mask & 4 == 0 { w } else { -w },
            ]);
        }

        let base = Config {
            preprocess: false,
            pure_literals: false,
            heuristic: Heuristic::Static,
            ..Config::default()
        };
        let learned = solve_here(&f, &base);
        let plain = solve_here(
            &f,
            &Config {
                learn: false,
                ..base.clone()
            },
        );
        assert!(matches!(learned.outcome, Outcome::Unsat));
        assert!(matches!(plain.outcome, Outcome::Unsat));

        let with = learned.stats.search.expect("search ran");
        let without = plain.stats.search.expect("search ran");
        assert!(with.learned > 0, "some clause should have been derived");
        assert!(
            with.nodes < without.nodes,
            "learning should save work: {} nodes with, {} without",
            with.nodes,
            without.nodes
        );
    }

    /// Adding derived clauses must not lose a model. Checking that a clause is implied by the
    /// formula is as expensive as solving, so this checks the consequence that matters.
    #[test]
    fn learning_does_not_lose_models() {
        let mut f = Cnf::new(0);
        for i in 1..=14_i32 {
            f.add_dimacs_clause(&[i, i % 14 + 1, -((i + 4) % 14 + 1)]);
            f.add_dimacs_clause(&[-i, (i + 2) % 14 + 1]);
        }
        for learn in [true, false] {
            let r = solve_here(
                &f,
                &Config {
                    learn,
                    preprocess: false,
                    ..Config::default()
                },
            );
            assert!(matches!(r.outcome, Outcome::Sat(_)), "learn={learn}");
            r.verify(&f)
                .expect("model must satisfy the original formula");
        }
    }

    /// A derived clause spans whatever variables the conflict touched, which can cut across the
    /// component it was found in. The memo keys on active clause indices, so those clauses have
    /// to appear in the component and in the key — otherwise a verdict is reused where it does
    /// not hold. This is the test that would catch that.
    #[test]
    fn learning_and_the_memo_agree_on_decomposable_formulas() {
        // Blocks that decompose, tied together by two long clauses so that a conflict inside one
        // block derives a clause mentioning another.
        let mut f = Cnf::new(0);
        for block in 0..5_i32 {
            let b = block * 4;
            f.add_dimacs_clause(&[b + 1, b + 2, b + 3]);
            f.add_dimacs_clause(&[-(b + 1), -(b + 2)]);
            f.add_dimacs_clause(&[-(b + 2), -(b + 3)]);
            f.add_dimacs_clause(&[-(b + 1), -(b + 3), b + 4]);
            f.add_dimacs_clause(&[-(b + 4), b + 2]);
        }
        f.add_dimacs_clause(&[1, 5, 9, 13, 17]);
        f.add_dimacs_clause(&[-1, -5, -9]);

        let mut verdicts = Vec::new();
        for learn in [true, false] {
            for cache in [true, false] {
                let r = solve_here(
                    &f,
                    &Config {
                        learn,
                        cache,
                        preprocess: false,
                        ..Config::default()
                    },
                );
                r.verify(&f).expect("model must satisfy");
                verdicts.push((
                    format!("learn={learn} cache={cache}"),
                    matches!(r.outcome, Outcome::Sat(_)),
                ));
            }
        }
        let first = verdicts[0].1;
        for (label, verdict) in &verdicts {
            assert_eq!(*verdict, first, "{label} disagreed");
        }
    }

    /// Discarding long derived clauses is a performance decision, not a semantic one: every
    /// limit, down to one that keeps nothing at all, must give the same verdict.
    #[test]
    fn the_learned_clause_size_limit_does_not_change_verdicts() {
        let mut f = Cnf::new(0);
        for i in 1..=13_i32 {
            f.add_dimacs_clause(&[i, -(i % 13 + 1), (i + 3) % 13 + 1]);
            f.add_dimacs_clause(&[-i, i % 13 + 1, -((i + 5) % 13 + 1)]);
            f.add_dimacs_clause(&[-i, -(i % 13 + 1), -((i + 7) % 13 + 1)]);
        }
        let baseline = verdict(&f, &Config::default());
        for limit in [1, 2, 3, 5, 12, 0] {
            let c = Config {
                max_learned_clause_size: limit,
                ..Config::default()
            };
            assert_eq!(verdict(&f, &c), baseline, "size limit {limit} disagreed");
        }
    }

    #[test]
    fn the_node_limit_yields_unknown() {
        // A formula big enough that one node cannot settle it.
        let mut f = Cnf::new(0);
        for i in 1..=30_i32 {
            f.add_dimacs_clause(&[i, -(i % 30 + 1)]);
            f.add_dimacs_clause(&[-i, i % 30 + 1]);
            f.add_dimacs_clause(&[i, i % 30 + 1]);
            f.add_dimacs_clause(&[-i, -(i % 30 + 1)]);
        }
        let config = Config {
            max_nodes: Some(1),
            preprocess: false,
            ..Config::default()
        };
        let r = solve_here(&f, &config);
        // Either it aborted, or it was settled outright before the limit could bite.
        if let Outcome::Unknown(reason) = r.outcome {
            assert_eq!(reason, AbortReason::NodeLimit);
        }
    }

    #[test]
    fn solve_uses_a_deep_stack() {
        // A chain long enough to recurse deeply; `solve` must not overflow where the default
        // stack would.
        let mut f = Cnf::new(0);
        for i in 1..2000_i32 {
            f.add_dimacs_clause(&[-i, i + 1]);
        }
        f.add_dimacs_clause(&[1]);
        let r = solve(
            &f,
            &Config {
                preprocess: false,
                ..Config::default()
            },
        );
        assert!(matches!(r.outcome, Outcome::Sat(_)));
        r.verify(&f).expect("model must satisfy");
    }
}
