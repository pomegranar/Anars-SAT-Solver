//! Solver configuration.

use crate::cache::{BucketKind, DEFAULT_BUDGET_BYTES, DEFAULT_TARGET_LOAD};
use crate::search::heuristic::Heuristic;
use std::time::Duration;

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
            pure_literals: true,
            preprocess: true,
            dp_clause_limit: 200_000,
            timeout: None,
            max_nodes: None,
        }
    }
}

impl Config {
    /// Configuration for plain DPLL, with every extra switched off.
    ///
    /// This is the control the memo is measured against, so it is a named constructor rather
    /// than something a benchmark script has to assemble correctly by hand.
    #[must_use]
    pub fn plain_dpll() -> Self {
        Self {
            algorithm: Algorithm::Dpll,
            cache: false,
            preprocess: false,
            ..Self::default()
        }
    }
}
