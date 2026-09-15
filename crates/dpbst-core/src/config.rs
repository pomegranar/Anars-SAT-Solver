//! Solver configuration.

use crate::cache::{BucketKind, DEFAULT_BUDGET_BYTES, DEFAULT_TARGET_LOAD};
use crate::search::heuristic::Heuristic;
use std::time::Duration;

/// Default longest derived clause to keep.
///
/// Three. That is far shorter than a CDCL solver would tolerate, and it is a consequence of
/// propagating by clause counters rather than watched literals: a stored clause is charged on
/// every assignment to every variable it mentions. Swept over the benchmark suite under PAR-2,
/// the limit is the difference between learning paying for itself and not; see the README.
pub const DEFAULT_MAX_LEARNED_CLAUSE_SIZE: usize = 3;

/// Default ceiling on derived clause literals: eight million, or about 32 MiB of them.
pub const DEFAULT_MAX_LEARNED_LITERALS: usize = 8 << 20;

/// Which algorithm to run.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Algorithm {
    /// DPLL search with connected-component decomposition and a memo. The point of the project.
    #[default]
    DpllMemo,
    /// Plain DPLL: no decomposition, no memo. The baseline the memo has to beat.
    Dpll,
    /// Davis and Putnam's original 1960 procedure: eliminate variables by resolution until the
    /// formula is empty or contains the empty clause. Included because it is the other thing
    /// "DP" means, and because watching it explode is instructive.
    DavisPutnam,
}

impl Algorithm {
    /// Every algorithm, in a stable order for benchmark tables.
    pub const ALL: [Self; 3] = [Self::DpllMemo, Self::Dpll, Self::DavisPutnam];

    /// The algorithm's CLI name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DpllMemo => "dpll-memo",
            Self::Dpll => "dpll",
            Self::DavisPutnam => "dp",
        }
    }
}

impl std::str::FromStr for Algorithm {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|a| a.name() == s)
            .ok_or_else(|| format!("unknown algorithm `{s}`"))
    }
}

impl std::fmt::Display for Algorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// How to run a solve.
#[derive(Clone, Debug)]
// Every technique the solver has is switchable, because every one of them is ablated in the
// benchmark tables. A row of independent booleans is what that actually is.
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    /// Which algorithm to run.
    pub algorithm: Algorithm,
    /// Bucket policy for the memo.
    pub bucket: BucketKind,
    /// Branching rule.
    pub heuristic: Heuristic,
    /// Whether to consult and populate the memo.
    pub cache: bool,
    /// Byte budget for the memo, after which it starts evicting.
    pub cache_budget_bytes: usize,
    /// Entries per bucket the memo aims for before growing.
    pub target_load: usize,
    /// Whether to derive a clause from every conflict and backjump on it.
    pub learn: bool,
    /// Longest derived clause worth keeping; longer ones are discarded and the search
    /// backtracks chronologically instead.
    ///
    /// Propagation here is by clause counters, not watched literals, so a stored clause costs
    /// work on every assignment to every variable it mentions. Long clauses are the worst of
    /// that trade — most cost, least pruning — and dropping them is what keeps learning a win on
    /// large instances. Zero means no limit.
    pub max_learned_clause_size: usize,
    /// Cap on the total literals held in derived clauses, after which learning stops.
    ///
    /// Learned clauses are never deleted — component keys name clauses by index, so recycling an
    /// index would change what an existing memo entry means — which is why the database needs a
    /// ceiling rather than a reduction policy. The default is generous enough that only a very
    /// long run on a very hard instance reaches it.
    pub max_learned_literals: usize,
    /// Whether to apply pure literal elimination inside components.
    pub pure_literals: bool,
    /// Whether to run the preprocessor.
    pub preprocess: bool,
    /// Cap on resolvents produced by [`Algorithm::DavisPutnam`] before giving up.
    pub dp_clause_limit: usize,
    /// Wall-clock limit for the solve.
    pub timeout: Option<Duration>,
    /// Cap on search nodes, mostly for tests that must terminate.
    pub max_nodes: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::default(),
            bucket: BucketKind::default(),
            heuristic: Heuristic::default(),
            cache: true,
            cache_budget_bytes: DEFAULT_BUDGET_BYTES,
            target_load: DEFAULT_TARGET_LOAD,
            learn: true,
            max_learned_clause_size: DEFAULT_MAX_LEARNED_CLAUSE_SIZE,
            max_learned_literals: DEFAULT_MAX_LEARNED_LITERALS,
            pure_literals: true,
            preprocess: true,
            dp_clause_limit: 200_000,
            timeout: None,
            max_nodes: None,
        }
    }
}

impl Config {
    /// Configuration for plain DPLL, with every extra switched off — no decomposition, no memo,
    /// no learning, no preprocessing.
    ///
    /// This is the control the memo is measured against, so it is a named constructor rather
    /// than something a benchmark script has to assemble correctly by hand.
    #[must_use]
    pub fn plain_dpll() -> Self {
        Self {
            algorithm: Algorithm::Dpll,
            cache: false,
            learn: false,
            preprocess: false,
            ..Self::default()
        }
    }
}
