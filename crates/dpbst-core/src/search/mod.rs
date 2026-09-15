//! The DPLL search, with component decomposition, clause learning and the memo wired in.
//!
//! # Learning inside a decomposing search
//!
//! Conflict-driven clause learning and component caching pull in opposite directions, and the
//! way they are reconciled here is the one thing in this module worth reading carefully.
//!
//! A learned clause is a resolvent of clauses already in the database, so it is implied by the
//! input formula. That makes it safe to *add*. It does not make it safe to ignore. Suppose a
//! learned clause were hidden from component analysis: assigning a variable of component `A`
//! could then falsify it and force a variable of component `B`, and the two components would no
//! longer be independent — the premise the whole memo rests on. Worse, a component could be
//! reported unsatisfiable on the strength of a clause that the memo key does not mention, and
//! that verdict would then be reused somewhere it does not hold.
//!
//! So learned clauses are ordinary clauses here. They appear in the occurrence lists, they take
//! part in decomposition, and their indices appear in component keys exactly like input clauses.
//! The key therefore still determines the residual formula, and every cached verdict stays true
//! for as long as the entry lives. The price is paid in hit rate: a component reached before a
//! clause was learned and after it gets two different names. That is the honest cost of putting
//! the two techniques in one solver, and the benchmark tables report it rather than hide it.
//!
//! Because keys name clauses by index, learned clauses are never deleted. Clause deletion would
//! either recycle an index — silently changing what an existing key means — or leave the memo
//! full of entries referring to clauses that no longer exist. Instead the database is capped by
//! [`crate::config::Config::max_learned_literals`], after which the search simply stops learning.
//!
//! # Backjumping through recursion
//!
//! The search is recursive: one frame per component, and one decision level per polarity a frame
//! tries. Non-chronological backjumping therefore has to travel back up the call stack. A frame
//! that derives a clause asserting at a shallower level returns [`Answer::Backjump`], and each
//! frame on the way out closes its own level and passes it on until the frame that owns the
//! target level is reached. That frame does not pop: it discards the subproblems it had split
//! off, propagates the newly learned clause — which is now unit — and splits again. The root
//! does the same at level zero.

pub mod component;
pub mod heuristic;
pub mod state;

use crate::cache::arena::Verdict;
use crate::cache::{BucketPolicy, CacheStats, ComponentCache, TableShape};
use crate::cnf::Cnf;
use crate::config::{Algorithm, Config};
use component::{ComponentAnalyzer, ComponentRef, ComponentStore};
use heuristic::DecisionMaker;
use state::{Conflict, SearchState};
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
    /// A clause was learned that asserts at a shallower decision level, and every frame down to
    /// the one owning that level must unwind. See the module notes.
    Backjump(usize),
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
    /// Clauses derived by conflict analysis and kept.
    pub learned: u64,
    /// Clauses derived and thrown away for being longer than the size limit.
    pub discarded: u64,
    /// Literals across all derived clauses.
    pub learned_literals: u64,
    /// Conflicts whose derived clause sent the search back past more than one decision level.
    pub backjumps: u64,
    /// Decision levels skipped by those backjumps.
    pub levels_skipped: u64,
    /// Deepest recursion reached.
    pub max_depth: u64,
    /// Memo counters.
    pub cache: CacheStats,
    /// Memo shape at the end of the run.
    pub table: TableShape,
    /// Peak bytes held by the memo.
    pub cache_bytes: usize,
}

/// How one polarity of one decision ended.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Branch {
    /// Every subproblem was satisfied.
    Sat,
    /// The branch failed here; try the other polarity.
    Failed,
    /// Unwind to this decision level, where a learned clause is waiting to propagate.
    Backjump(usize),
    /// A limit was hit.
    Aborted,
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
    /// Clauses learned since the last propagation, to be re-examined once the search has
    /// backtracked far enough for them to say something.
    pending: Vec<u32>,
    counters: SearchCounters,
    deadline: Option<Instant>,
    aborted: bool,
    /// Set when conflict analysis derived the empty clause: the formula is unsatisfiable, and
    /// the [`Answer::Backjump`] travelling up the stack means "stop", not "retry".
    root_unsat: bool,
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
            pending: Vec::new(),
            counters: SearchCounters::default(),
            deadline,
            aborted: false,
            root_unsat: false,
        }
    }

    /// The counters gathered so far.
    #[must_use]
    pub fn counters(&self) -> SearchCounters {
        let mut c = self.counters;
        c.propagations = self.state.propagations();
        c.learned_literals = self.state.learned_literals();
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
        if self.state.in_conflict() || !self.propagate() {
            return Answer::Unsat;
        }

        // The root owns decision level zero. A clause learned anywhere in the search that asserts
        // at level zero unwinds to here, where it is propagated and everything is split again.
        loop {
            self.scope.clear();
            self.scope.extend(0..self.state.num_vars() as u32);
            let scope_range = 0..self.scope.len();
            let store_mark = self.store.mark();
            let roots = self.decompose(scope_range);

            let mut restart = false;
            for index in roots {
                match self.solve_component(ComponentRef::new(index), 1) {
                    Answer::Sat => {}
                    Answer::Unsat => return Answer::Unsat,
                    Answer::Aborted => return Answer::Aborted,
                    Answer::Backjump(target) => {
                        debug_assert_eq!(target, 0, "the root owns level zero and nothing below");
                        if self.root_unsat {
                            return Answer::Unsat;
                        }
                        restart = true;
                        break;
                    }
                }
            }
            self.store.truncate(store_mark);

            if !restart {
                break;
            }
            if !self.propagate() {
                // A conflict with no decisions above it is the formula's own.
                return Answer::Unsat;
            }
        }

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
            let level = self.state.decision_level();
            self.state.enqueue(lit);

            match self.branch(comp, hash, depth, level) {
                Branch::Sat => {
                    self.state.pop_level();
                    return Answer::Sat;
                }
                Branch::Failed => self.state.pop_level(),
                Branch::Aborted => {
                    self.state.pop_level();
                    return Answer::Aborted;
                }
                Branch::Backjump(target) => {
                    debug_assert!(target < level, "the owning frame handles its own level");
                    self.state.pop_level();
                    return Answer::Backjump(target);
                }
            }
        }

        if self.config.cache {
            self.cache
                .insert(hash, self.store.key(comp), Verdict::Unsat, &[]);
        }
        Answer::Unsat
    }

    /// Explores one polarity of one decision, at the decision level `level` it opened.
    ///
    /// This is where a backjump lands. If a descendant asks to unwind to `level`, the work done
    /// under it is thrown away and the whole thing is redone with the newly learned clause in
    /// force; anything shallower is passed further up.
    fn branch(&mut self, comp: ComponentRef, hash: u64, depth: u64, level: usize) -> Branch {
        loop {
            if !self.propagate() {
                self.counters.conflicts += 1;
                match self.learn(level) {
                    Some(target) => return Branch::Backjump(target),
                    None => return Branch::Failed,
                }
            }

            let scope_mark = self.scope.len();
            self.scope.extend_from_slice(self.store.vars(comp));
            let store_mark = self.store.mark();
            let subs = self.decompose(scope_mark..self.scope.len());

            let mut outcome = Branch::Sat;
            for index in subs {
                match self.solve_component(ComponentRef::new(index), depth + 1) {
                    Answer::Sat => {}
                    Answer::Unsat => {
                        outcome = Branch::Failed;
                        break;
                    }
                    Answer::Aborted => {
                        outcome = Branch::Aborted;
                        break;
                    }
                    Answer::Backjump(target) => {
                        outcome = Branch::Backjump(target);
                        break;
                    }
                }
            }

            if outcome == Branch::Sat {
                // The witness has to be read off before the decision level is popped.
                self.capture(comp);
                if self.config.cache {
                    self.cache
                        .insert(hash, self.store.key(comp), Verdict::Sat, &self.witness);
                }
            }
            self.store.truncate(store_mark);
            self.scope.truncate(scope_mark);

            // A descendant asking to unwind to exactly this level lands here: stay put,
            // propagate what was learned, and split again. Anything else leaves the branch.
            match outcome {
                Branch::Backjump(target) if target == level && !self.root_unsat => {}
                other => return other,
            }
        }
    }

    /// Derives a clause from the current conflict.
    ///
    /// Returns the level to unwind to, or `None` when the search should simply try the other
    /// polarity: either learning is off, the database is full, or the derived clause is not
    /// asserting and so justifies no backjump.
    fn learn(&mut self, level: usize) -> Option<usize> {
        if !self.config.learn
            || self.state.learned_literals() >= self.config.max_learned_literals as u64
        {
            return None;
        }
        match self
            .state
            .analyze_conflict(self.config.max_learned_clause_size)
        {
            Conflict::TooLong(_) => {
                self.counters.discarded += 1;
                None
            }
            Conflict::Root => {
                self.root_unsat = true;
                Some(0)
            }
            Conflict::Learned {
                clause,
                assert_level,
                asserting,
            } => {
                self.counters.learned += 1;
                self.pending.push(clause);
                if !asserting || assert_level >= level {
                    return None;
                }
                if assert_level + 1 < level {
                    self.counters.backjumps += 1;
                    self.counters.levels_skipped += (level - assert_level - 1) as u64;
                }
                Some(assert_level)
            }
        }
    }

    /// Propagates to fixpoint, first re-offering any clause learned since the last call.
    ///
    /// Backtracking only ever unassigns, so it cannot make a clause unit by the ordinary route:
    /// a clause derived from a conflict becomes unit precisely *because* the search unwound past
    /// the assignments that falsified it, and nothing would otherwise look at it again.
    fn propagate(&mut self) -> bool {
        for c in self.pending.drain(..) {
            self.state.queue_clause(c);
        }
        self.state.propagate()
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
