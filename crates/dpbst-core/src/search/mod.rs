//! The DPLL search, with component decomposition and the memo wired in.

pub mod component;
pub mod heuristic;
pub mod state;

use crate::cache::arena::Verdict;
use crate::cache::{BucketPolicy, CacheStats, ComponentCache, TableShape};
use crate::cnf::Cnf;
use crate::config::{Algorithm, Config};
use component::{ComponentAnalyzer, ComponentRef, ComponentStore};
use heuristic::DecisionMaker;
use state::SearchState;
use std::ops::Range;
use std::time::Instant;

/// How often, in search nodes, to check the wall clock. Reading the clock is not free and the
/// answer changes slowly relative to node visits.
const CLOCK_CHECK_INTERVAL: u64 = 4096;

/// The result of solving one component, or the whole formula.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    /// Satisfiable; the witness has been written into the model.
    Sat,
    /// Unsatisfiable.
    Unsat,
    /// A limit was hit before an answer was found.
    Aborted,
}

/// Counters describing one search.
#[derive(Clone, Copy, Debug, Default)]
pub struct SearchCounters {
    /// Component solve attempts, including those answered by the memo.
    pub nodes: u64,
    /// Branching decisions taken.
    pub decisions: u64,
    /// Conflicts reached.
    pub conflicts: u64,
    /// Literal assignments, including propagated ones.
    pub propagations: u64,
    /// Components produced by decomposition.
    pub components: u64,
    /// Pure literals assigned.
    pub pure_literals: u64,
    /// Deepest recursion reached.
    pub max_depth: u64,
    /// Memo counters.
    pub cache: CacheStats,
    /// Memo shape at the end of the run.
    pub table: TableShape,
    /// Peak bytes held by the memo.
    pub cache_bytes: usize,
}

/// A DPLL search over one formula.
pub struct Searcher<'a, P: BucketPolicy> {
    state: SearchState,
    store: ComponentStore,
    analyzer: ComponentAnalyzer,
    cache: ComponentCache<P>,
    decider: DecisionMaker,
    config: &'a Config,
    /// The assignment being built. Component witnesses are written here as they are found.
    model: Vec<bool>,
    /// Stack of variable scopes; a node pushes its component's variables and pops them on exit.
    scope: Vec<u32>,
    /// Scratch for the witness bitset handed to the memo.
    witness: Vec<u8>,
    counters: SearchCounters,
    deadline: Option<Instant>,
    aborted: bool,
}

impl<P: BucketPolicy> std::fmt::Debug for Searcher<'_, P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Searcher")
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl<'a, P: BucketPolicy> Searcher<'a, P> {
    /// Prepares a search over `cnf`.
    #[must_use]
    pub fn new(cnf: &Cnf, config: &'a Config, deadline: Option<Instant>) -> Self {
        Self {
            state: SearchState::new(cnf),
            store: ComponentStore::new(),
            analyzer: ComponentAnalyzer::new(cnf.num_vars(), cnf.num_clauses()),
            cache: ComponentCache::new(config.cache_budget_bytes, config.target_load),
            decider: DecisionMaker::new(cnf.num_vars()),
            config,
            model: vec![false; cnf.num_vars()],
            scope: Vec::new(),
            witness: Vec::new(),
            counters: SearchCounters::default(),
            deadline,
            aborted: false,
        }
    }

    /// The counters gathered so far.
    #[must_use]
    pub fn counters(&self) -> SearchCounters {
        let mut c = self.counters;
        c.propagations = self.state.propagations();
        c.cache = self.cache.stats();
        c.table = self.cache.shape();
        c.cache_bytes = self.cache.memory_bytes();
        c
    }

    /// The assignment found, valid only after [`Searcher::run`] returns [`Answer::Sat`].
    #[must_use]
    pub fn model(&self) -> &[bool] {
        &self.model
    }

    /// Runs the search to completion.
    pub fn run(&mut self) -> Answer {
        if self.state.in_conflict() || !self.state.propagate() {
            return Answer::Unsat;
        }

        self.scope.extend(0..self.state.num_vars() as u32);
        let scope_range = 0..self.scope.len();
        let store_mark = self.store.mark();
        let roots = self.decompose(scope_range);

        for index in roots {
            match self.solve_component(ComponentRef::new(index), 1) {
                Answer::Sat => {}
                other => return other,
            }
        }
        self.store.truncate(store_mark);

        // Pick up everything fixed by top-level propagation and by pure literals. Variables that
        // appear in no active clause keep their default, which is always sound because every
        // clause containing them is satisfied.
        for v in 0..self.state.num_vars() {
            if let Some(value) = self.state.value_of_index(v) {
                self.model[v] = value;
            }
        }
        Answer::Sat
    }

    /// Solves one component, consulting and populating the memo.
    fn solve_component(&mut self, comp: ComponentRef, depth: u64) -> Answer {
        self.counters.nodes += 1;
        self.counters.max_depth = self.counters.max_depth.max(depth);
        if self.should_abort() {
            return Answer::Aborted;
        }

        let hash = self.store.hash(comp);
        if self.config.cache {
            if let Some(node) = self.cache.lookup(hash, self.store.key(comp)) {
                return match self.cache.verdict(node) {
                    Verdict::Unsat => Answer::Unsat,
                    Verdict::Sat => {
                        self.apply_witness(comp, node);
                        Answer::Sat
                    }
                };
            }
        }

        let Some(decision) =
            self.decider
                .pick(&self.state, &self.store, comp, self.config.heuristic)
        else {
            // A component always carries at least one active clause, and an active clause always
            // has an unassigned literal, so this is unreachable in practice.
            return Answer::Sat;
        };

        for lit in [decision, !decision] {
            self.counters.decisions += 1;
            self.state.push_level();
            self.state.enqueue(lit);

            if self.state.propagate() {
                let scope_mark = self.scope.len();
                self.scope.extend_from_slice(self.store.vars(comp));
                let store_mark = self.store.mark();

                let subs = self.decompose(scope_mark..self.scope.len());
                let mut all_sat = true;
                for index in subs {
                    match self.solve_component(ComponentRef::new(index), depth + 1) {
                        Answer::Sat => {}
                        Answer::Unsat => {
                            all_sat = false;
                            break;
                        }
                        Answer::Aborted => {
                            self.state.pop_level();
                            return Answer::Aborted;
                        }
                    }
                }

                if all_sat {
                    // The witness has to be read off before the decision level is popped.
                    self.capture(comp);
                    if self.config.cache {
                        self.cache
                            .insert(hash, self.store.key(comp), Verdict::Sat, &self.witness);
                    }
                    self.store.truncate(store_mark);
                    self.scope.truncate(scope_mark);
                    self.state.pop_level();
                    return Answer::Sat;
                }

                self.store.truncate(store_mark);
                self.scope.truncate(scope_mark);
            } else {
                self.counters.conflicts += 1;
            }
            self.state.pop_level();
        }

        if self.config.cache {
            self.cache
                .insert(hash, self.store.key(comp), Verdict::Unsat, &[]);
        }
        Answer::Unsat
    }

    /// Splits a scope into components, first driving pure literal elimination to fixpoint.
    ///
    /// Purity is computed as a side effect of the same scan that finds the components, so the
    /// fixpoint loop costs one extra pass per round of pure literals rather than a separate
    /// sweep of the formula.
    fn decompose(&mut self, scope: Range<usize>) -> Range<usize> {
        loop {
            let mark = self.store.mark();
            let detect_pure = self.config.pure_literals;
            let range = if self.config.algorithm == Algorithm::Dpll {
                self.analyzer.analyze_whole(
                    &self.state,
                    &self.scope[scope.clone()],
                    &mut self.store,
                    detect_pure,
                )
            } else {
                self.analyzer.analyze(
                    &self.state,
                    &self.scope[scope.clone()],
                    &mut self.store,
                    detect_pure,
                )
            };

            if !detect_pure || self.analyzer.pure_literals().is_empty() {
                self.counters.components += range.len() as u64;
                return range;
            }

            // Assigning a pure literal can only satisfy clauses, never shorten one, so it can
            // neither conflict nor propagate. The components just computed are stale, though, so
            // they are discarded and the scan repeats.
            self.store.truncate(mark);
            let mut assigned = 0;
            for i in 0..self.analyzer.pure_literals().len() {
                let lit = self.analyzer.pure_literals()[i];
                if self.state.value(lit.var()).is_none() {
                    self.state.enqueue(lit);
                    assigned += 1;
                }
            }
            self.counters.pure_literals += assigned;
            debug_assert!(
                !self.state.in_conflict(),
                "pure literal elimination must not cause a conflict"
            );
            if assigned == 0 {
                // Defensive: without progress this would spin.
                self.counters.components += range.len() as u64;
                return range;
            }
        }
    }

    /// Reads a solved component's assignment out of the search state and into the model and the
    /// witness buffer.
    ///
    /// Variables left unassigned are either free — every clause holding them is satisfied, so any
    /// value works — or were filled in by a memo hit deeper down. Either way the value already in
    /// `model` is correct, which is why this only overwrites variables the state has a value for.
    fn capture(&mut self, comp: ComponentRef) {
        let count = self.store.vars(comp).len();
        self.witness.clear();
        self.witness.resize(count.div_ceil(8), 0);
        for i in 0..count {
            let v = self.store.vars(comp)[i] as usize;
            if let Some(value) = self.state.value_of_index(v) {
                self.model[v] = value;
            }
            if self.model[v] {
                self.witness[i / 8] |= 1 << (i % 8);
            }
        }
    }

    /// Writes a memo hit's stored witness into the model.
    fn apply_witness(&mut self, comp: ComponentRef, node: crate::cache::arena::NodeId) {
        let count = self.store.vars(comp).len();
        for i in 0..count {
            let v = self.store.vars(comp)[i] as usize;
            let bit = self.cache.witness(node)[i / 8] >> (i % 8) & 1;
            self.model[v] = bit == 1;
        }
    }

    /// Whether a node or time limit has been reached.
    fn should_abort(&mut self) -> bool {
        if self.aborted {
            return true;
        }
        if let Some(limit) = self.config.max_nodes
            && self.counters.nodes > limit
        {
            self.aborted = true;
            return true;
        }
        if self.counters.nodes % CLOCK_CHECK_INTERVAL == 0
            && let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            self.aborted = true;
            return true;
        }
        false
    }
}
