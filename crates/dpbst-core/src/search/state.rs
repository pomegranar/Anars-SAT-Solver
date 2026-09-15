//! The mutable search state: assignment, trail, clause counters, and conflict analysis.
//!
//! # Why counters instead of watched literals
//!
//! Every competitive CDCL solver propagates with two watched literals, because it lets a clause
//! be ignored entirely until one of two specific literals is falsified. It has one property that
//! rules it out here: it cannot tell you whether a clause is *satisfied*.
//!
//! Component analysis runs at every search node and must enumerate the clauses that are still
//! active, which means asking exactly that question about every clause a variable occurs in. So
//! this solver maintains, per clause:
//!
//! * `sat_count` — literals currently true,
//! * `unassigned_count` — literals whose variable is still unassigned,
//!
//! from which satisfied / active / unit / conflicting all fall out in constant time, and which
//! undo exactly on backtracking. The cost is real: assigning a variable touches every clause it
//! occurs in, where watched literals would touch a fraction of them. That trade is the single
//! biggest reason this solver is slower than `CaDiCaL` per propagation, and it is made on purpose.
//!
//! # The clause database has two halves
//!
//! Clauses `0 .. num_original` come from the input and never move, so their occurrence lists are
//! two flat arrays built once by counting sort. Clauses learned from conflicts are appended, and
//! their occurrences live in a per-literal `Vec` that can grow. Assigning a literal walks both.
//! Keeping the original half flat matters because it is the overwhelming majority of the work on
//! every instance where learning does not run away.
//!
//! # Conflict analysis
//!
//! [`SearchState::analyze_conflict`] implements first-UIP learning: resolve the conflicting
//! clause against the reasons of the literals that caused it, most recent first, until one
//! literal from the conflict level remains. The result is a resolvent of clauses already in the
//! database and therefore implied by the input formula, which is what makes it safe to add.
//!
//! One wrinkle is local to this solver. Pure literals are assigned *without* a reason — purity
//! is a satisfiability-preserving simplification, not an implication — so unlike a textbook CDCL
//! solver there can be several reason-less assignments on one decision level. Resolution stops
//! at each of them and keeps the literal, exactly as it stops at a decision. The derived clause
//! is still a resolvent; it is simply not always asserting, and [`Conflict::asserting`] says so.

use crate::cnf::Cnf;
use crate::lit::{Lit, Var};

/// Marks a variable that was assigned by a decision or by pure literal elimination, neither of
/// which has a reason clause.
const NO_REASON: u32 = u32::MAX;

/// What conflict analysis concluded.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Conflict {
    /// Every literal of the conflicting clause is false at the root. Nothing can undo that, so
    /// the formula is unsatisfiable outright.
    Root,
    /// A clause was derived, but it was longer than the caller was willing to keep, so it was
    /// discarded. Nothing was added and no backjump is justified.
    TooLong(usize),
    /// A clause was derived and added to the database.
    Learned {
        /// Index of the derived clause.
        clause: u32,
        /// The decision level the search must return to for the clause to do any work. When the
        /// clause is asserting this is where its remaining literal becomes unit; otherwise it is
        /// simply the conflict level, meaning "no backjump is justified".
        assert_level: usize,
        /// Whether exactly one literal of the derived clause sits at the conflict level. Only
        /// then does backtracking to `assert_level` force an assignment; see the module notes on
        /// pure literals for why this is not always the case here.
        asserting: bool,
    },
}

/// Assignment, trail, clause counters and conflict analysis for one solve.
#[derive(Debug)]
pub struct SearchState {
    num_vars: usize,
    /// Clauses `0..num_original` came from the input; the rest were learned.
    num_original: usize,

    // --- clause database, flattened ------------------------------------------------------------
    lits: Vec<Lit>,
    /// `clause_bounds[c]..clause_bounds[c + 1]` is clause `c`.
    clause_bounds: Vec<u32>,
    /// `occ_bounds[l]..occ_bounds[l + 1]` indexes `occ`, giving the *original* clauses containing
    /// literal `l`.
    occ_bounds: Vec<u32>,
    occ: Vec<u32>,
    /// Learned clauses containing each literal. Separate from `occ` so that the original half
    /// stays two flat arrays; see the module notes.
    learned_occ: Vec<Vec<u32>>,

    // --- dynamic state -------------------------------------------------------------------------
    /// True literals per clause. Zero means the clause is still active.
    sat_count: Vec<u32>,
    /// Unassigned literals per clause.
    unassigned_count: Vec<u32>,
    value: Vec<Option<bool>>,
    /// Decision level at which each variable was assigned; meaningless while unassigned.
    level: Vec<u32>,
    /// The clause that forced each variable, or [`NO_REASON`] for a decision or a pure literal.
    reason: Vec<u32>,
    trail: Vec<Lit>,
    trail_lim: Vec<usize>,
    /// Clauses observed to have become unit, to be drained by [`SearchState::propagate`].
    unit_queue: Vec<u32>,
    conflict: bool,
    /// The clause that went empty, valid while `conflict` is set.
    conflict_clause: u32,

    // --- conflict analysis scratch -------------------------------------------------------------
    /// Stamp per variable, marking membership of the clause being derived. Zero is "not marked",
    /// so a pass costs a counter bump rather than a clear.
    seen: Vec<u32>,
    seen_stamp: u32,
    /// The clause being derived, reused across conflicts.
    derived: Vec<Lit>,

    // --- counters ------------------------------------------------------------------------------
    propagations: u64,
    learned_literals: u64,
}

impl SearchState {
    /// Builds the state for a formula, seeding the propagation queue with its unit clauses.
    ///
    /// An empty clause in the input puts the state into conflict immediately.
    #[must_use]
    pub fn new(cnf: &Cnf) -> Self {
        let num_vars = cnf.num_vars();
        let num_clauses = cnf.num_clauses();

        let mut lits = Vec::with_capacity(cnf.num_lits());
        let mut clause_bounds = Vec::with_capacity(num_clauses + 1);
        clause_bounds.push(0);
        for clause in cnf.clauses() {
            lits.extend_from_slice(clause);
            clause_bounds.push(lits.len() as u32);
        }

        // Occurrence lists, built with a counting sort so the whole index is two passes and two
        // allocations rather than a `Vec<Vec<_>>`.
        let mut occ_bounds = vec![0_u32; 2 * num_vars + 1];
        for &l in &lits {
            occ_bounds[l.index() + 1] += 1;
        }
        for i in 0..2 * num_vars {
            occ_bounds[i + 1] += occ_bounds[i];
        }
        let mut cursor = occ_bounds.clone();
        let mut occ = vec![0_u32; lits.len()];
        for c in 0..num_clauses {
            let start = clause_bounds[c] as usize;
            let end = clause_bounds[c + 1] as usize;
            for &l in &lits[start..end] {
                occ[cursor[l.index()] as usize] = c as u32;
                cursor[l.index()] += 1;
            }
        }

        let mut unassigned_count = Vec::with_capacity(num_clauses);
        for c in 0..num_clauses {
            unassigned_count.push(clause_bounds[c + 1] - clause_bounds[c]);
        }

        let mut state = Self {
            num_vars,
            num_original: num_clauses,
            lits,
            clause_bounds,
            occ_bounds,
            occ,
            learned_occ: vec![Vec::new(); 2 * num_vars],
            sat_count: vec![0; num_clauses],
            unassigned_count,
            value: vec![None; num_vars],
            level: vec![0; num_vars],
            reason: vec![NO_REASON; num_vars],
            trail: Vec::with_capacity(num_vars),
            trail_lim: Vec::new(),
            unit_queue: Vec::new(),
            conflict: false,
            conflict_clause: NO_REASON,
            seen: vec![0; num_vars],
            seen_stamp: 0,
            derived: Vec::new(),
            propagations: 0,
            learned_literals: 0,
        };

        for c in 0..num_clauses {
            match state.unassigned_count[c] {
                0 => state.record_conflict(c),
                1 => state.unit_queue.push(c as u32),
                _ => {}
            }
        }
        state
    }

    /// Number of variables.
    #[inline]
    #[must_use]
    pub const fn num_vars(&self) -> usize {
        self.num_vars
    }

    /// Number of clauses, learned ones included.
    #[inline]
    #[must_use]
    pub fn num_clauses(&self) -> usize {
        self.clause_bounds.len() - 1
    }

    /// Number of clauses that came from the input.
    #[inline]
    #[must_use]
    pub const fn num_original_clauses(&self) -> usize {
        self.num_original
    }

    /// Number of clauses derived from conflicts.
    #[inline]
    #[must_use]
    pub fn num_learned_clauses(&self) -> usize {
        self.num_clauses() - self.num_original
    }

    /// Total literals across all learned clauses, which is what bounds their memory.
    #[inline]
    #[must_use]
    pub const fn learned_literals(&self) -> u64 {
        self.learned_literals
    }

    /// Unit propagations performed so far.
    #[inline]
    #[must_use]
    pub const fn propagations(&self) -> u64 {
        self.propagations
    }

    /// The literals of clause `c`.
    #[inline]
    #[must_use]
    pub fn clause(&self, c: usize) -> &[Lit] {
        let start = self.clause_bounds[c] as usize;
        let end = self.clause_bounds[c + 1] as usize;
        &self.lits[start..end]
    }

    /// The input clauses containing literal `l`.
    #[inline]
    #[must_use]
    pub fn original_occurrences(&self, l: Lit) -> &[u32] {
        let start = self.occ_bounds[l.index()] as usize;
        let end = self.occ_bounds[l.index() + 1] as usize;
        &self.occ[start..end]
    }

    /// The learned clauses containing literal `l`.
    #[inline]
    #[must_use]
    pub fn learned_occurrences(&self, l: Lit) -> &[u32] {
        &self.learned_occ[l.index()]
    }

    /// Every clause containing literal `l`, input clauses first.
    ///
    /// Component analysis and the branching heuristics walk this, so a learned clause takes part
    /// in decomposition exactly as an input clause does. That is not an accident: propagation
    /// inside a component must stay inside it, and a learned clause hidden from the split would
    /// break that, and with it the memo.
    #[inline]
    pub fn occurrences(&self, l: Lit) -> impl Iterator<Item = u32> + '_ {
        self.original_occurrences(l)
            .iter()
            .chain(self.learned_occ[l.index()].iter())
            .copied()
    }

    /// The value of a variable, if assigned.
    #[inline]
    #[must_use]
    pub fn value(&self, var: Var) -> Option<bool> {
        self.value[var.index()]
    }

    /// The value of a variable by raw index, if assigned.
    #[inline]
    #[must_use]
    pub fn value_of_index(&self, var: usize) -> Option<bool> {
        self.value[var]
    }

    /// Whether clause `c` is satisfied under the current assignment.
    #[inline]
    #[must_use]
    pub fn is_satisfied(&self, c: usize) -> bool {
        self.sat_count[c] > 0
    }

    /// Whether clause `c` is still part of the residual formula.
    #[inline]
    #[must_use]
    pub fn is_active(&self, c: usize) -> bool {
        self.sat_count[c] == 0
    }

    /// Whether the state is in conflict.
    #[inline]
    #[must_use]
    pub const fn in_conflict(&self) -> bool {
        self.conflict
    }

    /// The current decision level.
    #[inline]
    #[must_use]
    pub fn decision_level(&self) -> usize {
        self.trail_lim.len()
    }

    /// Number of assigned variables.
    #[inline]
    #[must_use]
    pub fn trail_len(&self) -> usize {
        self.trail.len()
    }

    /// Assigns `lit` to true with no reason: a decision, or a pure literal.
    ///
    /// # Panics
    /// Panics in debug builds if the variable is already assigned.
    pub fn enqueue(&mut self, lit: Lit) {
        self.assign(lit, NO_REASON);
    }

    /// Assigns `lit` to true because clause `reason` forced it.
    ///
    /// # Panics
    /// Panics in debug builds if the variable is already assigned.
    pub fn enqueue_implied(&mut self, lit: Lit, reason: u32) {
        self.assign(lit, reason);
    }

    /// Assigns `lit` to true and updates every clause it occurs in.
    ///
    /// Clauses that become unit are queued; a clause that becomes empty sets the conflict flag.
    /// Counters are always brought fully up to date even when a conflict is detected part way,
    /// so that [`SearchState::pop_level`] can undo the assignment exactly.
    fn assign(&mut self, lit: Lit, reason: u32) {
        debug_assert!(
            self.value(lit.var()).is_none(),
            "{lit:?} is already assigned"
        );
        let var = lit.var().index();
        self.value[var] = Some(lit.is_positive());
        self.level[var] = self.trail_lim.len() as u32;
        self.reason[var] = reason;
        self.trail.push(lit);
        self.propagations += 1;

        // Clauses this literal satisfies.
        let (start, end) = self.occ_range(lit);
        for k in start..end {
            let c = self.occ[k] as usize;
            self.sat_count[c] += 1;
            self.unassigned_count[c] -= 1;
        }
        for i in 0..self.learned_occ[lit.index()].len() {
            let c = self.learned_occ[lit.index()][i] as usize;
            self.sat_count[c] += 1;
            self.unassigned_count[c] -= 1;
        }

        // Clauses this literal shortens.
        let (start, end) = self.occ_range(!lit);
        for k in start..end {
            let c = self.occ[k] as usize;
            self.shorten(c);
        }
        for i in 0..self.learned_occ[(!lit).index()].len() {
            let c = self.learned_occ[(!lit).index()][i] as usize;
            self.shorten(c);
        }
    }

    /// Accounts for one literal of clause `c` becoming false.
    #[inline]
    fn shorten(&mut self, c: usize) {
        self.unassigned_count[c] -= 1;
        if self.sat_count[c] == 0 {
            match self.unassigned_count[c] {
                0 => self.record_conflict(c),
                1 => self.unit_queue.push(c as u32),
                _ => {}
            }
        }
    }

    /// Records that clause `c` has no literal left to satisfy it.
    ///
    /// The *first* such clause is kept: assignment carries on after a conflict so that the undo
    /// stays exact, and it is the clause that actually failed which conflict analysis needs.
    #[inline]
    fn record_conflict(&mut self, c: usize) {
        if !self.conflict {
            self.conflict = true;
            self.conflict_clause = c as u32;
        }
    }

    #[inline]
    fn occ_range(&self, l: Lit) -> (usize, usize) {
        (
            self.occ_bounds[l.index()] as usize,
            self.occ_bounds[l.index() + 1] as usize,
        )
    }

    /// Queues clause `c` to be re-examined by the next [`SearchState::propagate`].
    ///
    /// Backtracking cannot make a clause unit by itself — it only unassigns — so a clause that
    /// became unit as a *consequence* of backtracking past the conflict that derived it is never
    /// noticed by the ordinary path. The search hands those here.
    pub fn queue_clause(&mut self, c: u32) {
        self.unit_queue.push(c);
    }

    /// Runs unit propagation to fixpoint.
    ///
    /// Returns `false` on conflict, in which case the caller must analyse or
    /// [`SearchState::pop_level`] before doing anything else.
    pub fn propagate(&mut self) -> bool {
        while !self.conflict {
            let Some(c) = self.unit_queue.pop() else {
                break;
            };
            let c = c as usize;
            // The queue records clauses that *were* unit; by the time one is drained it may have
            // been satisfied or shortened further, so re-check rather than trust the entry.
            if self.sat_count[c] > 0 {
                continue;
            }
            if self.unassigned_count[c] == 0 {
                self.record_conflict(c);
                break;
            }
            if self.unassigned_count[c] != 1 {
                continue;
            }
            let implied = self
                .clause(c)
                .iter()
                .copied()
                .find(|l| self.value[l.var().index()].is_none())
                .expect("a clause with one unassigned literal has one unassigned literal");
            self.enqueue_implied(implied, c as u32);
        }

        if self.conflict {
            self.unit_queue.clear();
            return false;
        }
        true
    }

    /// Opens a new decision level.
    pub fn push_level(&mut self) {
        self.trail_lim.push(self.trail.len());
    }

    /// Closes the innermost decision level, undoing every assignment made within it.
    ///
    /// # Panics
    /// Panics if there is no open decision level.
    pub fn pop_level(&mut self) {
        let target = self
            .trail_lim
            .pop()
            .expect("pop_level without a matching push_level");
        self.unassign_to(target);
        self.conflict = false;
        self.conflict_clause = NO_REASON;
        self.unit_queue.clear();
    }

    fn unassign_to(&mut self, target: usize) {
        while self.trail.len() > target {
            let lit = self.trail.pop().expect("trail is longer than the target");
            let (start, end) = self.occ_range(lit);
            for k in start..end {
                let c = self.occ[k] as usize;
                self.sat_count[c] -= 1;
                self.unassigned_count[c] += 1;
            }
            for i in 0..self.learned_occ[lit.index()].len() {
                let c = self.learned_occ[lit.index()][i] as usize;
                self.sat_count[c] -= 1;
                self.unassigned_count[c] += 1;
            }
            let (start, end) = self.occ_range(!lit);
            for k in start..end {
                self.unassigned_count[self.occ[k] as usize] += 1;
            }
            for i in 0..self.learned_occ[(!lit).index()].len() {
                let c = self.learned_occ[(!lit).index()][i] as usize;
                self.unassigned_count[c] += 1;
            }
            let var = lit.var().index();
            self.value[var] = None;
            self.reason[var] = NO_REASON;
        }
    }

    /// Appends a clause to the database, returning its index.
    ///
    /// Counters are computed against the assignment in force right now and maintained
    /// incrementally from then on, so a clause added mid-search stays consistent through every
    /// later assignment and undo. Clauses are never removed, which is what lets the component
    /// memo key itself on clause indices.
    ///
    /// # Panics
    /// Panics if the clause is empty; an empty derived clause means the formula is unsatisfiable
    /// and is reported as [`Conflict::Root`] instead.
    pub fn add_clause(&mut self, clause: &[Lit]) -> u32 {
        assert!(!clause.is_empty(), "the empty clause is not storable");
        let index = self.num_clauses() as u32;
        let mut sat = 0;
        let mut free = 0;
        for &l in clause {
            match self.value(l.var()) {
                Some(value) if value == l.is_positive() => sat += 1,
                Some(_) => {}
                None => free += 1,
            }
            self.learned_occ[l.index()].push(index);
        }
        self.lits.extend_from_slice(clause);
        self.clause_bounds.push(self.lits.len() as u32);
        self.sat_count.push(sat);
        self.unassigned_count.push(free);
        self.learned_literals += clause.len() as u64;
        index
    }

    /// Derives a clause from the current conflict by first-UIP resolution and adds it.
    ///
    /// A derived clause longer than `max_size` is thrown away rather than stored, and reported as
    /// [`Conflict::TooLong`]. That is a concession to this solver's propagation: a clause costs
    /// work on every assignment to every variable it mentions, where a watched-literal solver
    /// would leave it alone until one of two literals is touched. Long clauses are where that
    /// asymmetry bites hardest and where the pruning is worth least.
    ///
    /// # Panics
    /// Panics if the state is not in conflict.
    pub fn analyze_conflict(&mut self, max_size: usize) -> Conflict {
        assert!(self.conflict, "analyze_conflict without a conflict");
        let conflict_clause = self.conflict_clause as usize;

        // Which level the conflict really belongs to. Propagation runs to fixpoint after every
        // assignment, so this is all but always the current decision level; a clause learned
        // earlier can however be falsified entirely below it, and then the honest answer is the
        // deepest level its literals occupy.
        let conflict_level = self
            .clause(conflict_clause)
            .iter()
            .map(|l| self.level[l.var().index()])
            .max()
            .unwrap_or(0) as usize;
        if conflict_level == 0 {
            return Conflict::Root;
        }

        self.bump_seen();
        let mut derived = std::mem::take(&mut self.derived);
        derived.clear();

        let mut source = Some(conflict_clause);
        // The literal just resolved away; it is in both clauses and must not come back.
        let mut pivot: Option<Var> = None;
        // Literals at the conflict level that are still candidates for resolution.
        let mut pending = 0_usize;
        let mut index = self.trail.len();

        let uip = loop {
            if let Some(c) = source.take() {
                for k in self.clause_bounds[c] as usize..self.clause_bounds[c + 1] as usize {
                    let l = self.lits[k];
                    let var = l.var().index();
                    if Some(l.var()) == pivot
                        || self.seen[var] == self.seen_stamp
                        || self.level[var] == 0
                    {
                        continue;
                    }
                    self.seen[var] = self.seen_stamp;
                    if self.level[var] as usize == conflict_level {
                        pending += 1;
                    } else {
                        derived.push(l);
                    }
                }
            }

            // The most recently assigned literal still awaiting resolution. Reasons only ever
            // mention literals assigned earlier, so this scan never has to go back up.
            let p = loop {
                index -= 1;
                let l = self.trail[index];
                let var = l.var().index();
                if self.seen[var] == self.seen_stamp && self.level[var] as usize == conflict_level {
                    break l;
                }
            };

            if pending == 1 {
                break !p;
            }
            self.seen[p.var().index()] = 0;
            pending -= 1;
            match self.reason[p.var().index()] {
                // A decision or a pure literal: there is no clause to resolve against, so the
                // literal stays in the derived clause and resolution moves on.
                NO_REASON => derived.push(!p),
                reason => {
                    source = Some(reason as usize);
                    pivot = Some(p.var());
                }
            }
        };

        // Minimisation wants every literal of the derived clause marked, and resolution cleared
        // the marks of the ones it stopped at.
        for &l in &derived {
            self.seen[l.var().index()] = self.seen_stamp;
        }
        self.seen[uip.var().index()] = self.seen_stamp;
        self.minimize(&mut derived);

        let assert_level = derived
            .iter()
            .map(|l| self.level[l.var().index()] as usize)
            .max()
            .unwrap_or(0);
        let asserting = assert_level < conflict_level;

        // `derived` is still one literal short of the finished clause, which is the UIP.
        let length = derived.len() + 1;
        if max_size != 0 && length > max_size {
            self.derived = derived;
            return Conflict::TooLong(length);
        }

        // The asserting literal goes first: propagation scans a clause front to back looking for
        // its one unassigned literal, and this is it.
        derived.push(uip);
        let last = derived.len() - 1;
        derived.swap(0, last);

        let clause = self.add_clause(&derived);
        self.derived = derived;
        Conflict::Learned {
            clause,
            assert_level,
            asserting,
        }
    }

    /// Drops literals that the rest of the clause already implies.
    ///
    /// A literal is redundant when every literal of the clause that forced it is itself in the
    /// clause: resolving it away shortens the clause without weakening it. This is the
    /// non-recursive half of `MiniSat`'s conflict clause minimisation, which is cheap enough to
    /// be worth doing unconditionally.
    fn minimize(&self, derived: &mut Vec<Lit>) {
        derived.retain(|&l| {
            let reason = self.reason[l.var().index()];
            if reason == NO_REASON {
                return true;
            }
            !self.clause(reason as usize).iter().all(|&r| {
                r.var() == l.var()
                    || self.level[r.var().index()] == 0
                    || self.seen[r.var().index()] == self.seen_stamp
            })
        });
    }

    /// Advances the analysis stamp, clearing the marks wholesale on wrap-around.
    ///
    /// Stamps start at one so that zero always means "not marked", which is what lets resolution
    /// unmark a literal by writing a zero.
    fn bump_seen(&mut self) {
        if self.seen_stamp == u32::MAX {
            self.seen.fill(0);
            self.seen_stamp = 0;
        }
        self.seen_stamp += 1;
    }

    /// Debug-only audit that the counters agree with the assignment.
    ///
    /// Every counter here is maintained incrementally across millions of assign/undo pairs, so a
    /// single missed decrement would silently corrupt the search. This recomputes them from
    /// scratch and is called from tests and under the `paranoid` feature.
    #[must_use]
    pub fn counters_are_consistent(&self) -> bool {
        (0..self.num_clauses()).all(|c| {
            let clause = self.clause(c);
            let sat = clause
                .iter()
                .filter(|l| self.value(l.var()) == Some(l.is_positive()))
                .count();
            let free = clause
                .iter()
                .filter(|l| self.value(l.var()).is_none())
                .count();
            sat == self.sat_count[c] as usize && free == self.unassigned_count[c] as usize
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cnf::Cnf;

    fn cnf(clauses: &[&[i32]]) -> Cnf {
        let mut f = Cnf::new(0);
        for c in clauses {
            f.add_dimacs_clause(c);
        }
        f
    }

    fn lit(n: i32) -> Lit {
        Lit::from_dimacs(n)
    }

    fn occurrences(s: &SearchState, n: i32) -> Vec<u32> {
        s.occurrences(lit(n)).collect()
    }

    #[test]
    fn occurrence_lists_are_complete() {
        let s = SearchState::new(&cnf(&[&[1, 2], &[-1, 3], &[1, -3]]));
        assert_eq!(occurrences(&s, 1), vec![0, 2]);
        assert_eq!(occurrences(&s, -1), vec![1]);
        assert_eq!(occurrences(&s, 3), vec![1]);
        assert_eq!(occurrences(&s, -3), vec![2]);
        assert_eq!(occurrences(&s, -2), vec![]);
    }

    /// A learned clause has to show up in the occurrence lists, or component analysis would not
    /// see it and propagation inside a component could escape it.
    #[test]
    fn learned_clauses_join_the_occurrence_lists() {
        let mut s = SearchState::new(&cnf(&[&[1, 2], &[-1, 3]]));
        let c = s.add_clause(&[lit(-2), lit(3)]);
        assert_eq!(c, 2);
        assert_eq!(occurrences(&s, 3), vec![1, 2]);
        assert_eq!(occurrences(&s, -2), vec![2]);
        assert_eq!(s.num_original_clauses(), 2);
        assert_eq!(s.num_learned_clauses(), 1);
        assert!(s.counters_are_consistent());
    }

    /// Counters for a clause added mid-search must account for the assignment already in force,
    /// and must then undo exactly like any other clause.
    #[test]
    fn a_clause_added_mid_search_undoes_correctly() {
        let mut s = SearchState::new(&cnf(&[&[1, 2], &[-1, 3]]));
        s.push_level();
        s.enqueue(lit(1));
        assert!(s.propagate());
        let c = s.add_clause(&[lit(-1), lit(-3), lit(2)]);
        assert!(s.counters_are_consistent(), "counters at insertion time");
        assert!(s.is_active(c as usize), "-1 and -3 are false, 2 is free");
        s.pop_level();
        assert!(s.counters_are_consistent(), "counters after backtracking");
        assert_eq!(s.trail_len(), 0);
    }

    #[test]
    fn assigning_updates_both_counters() {
        let mut s = SearchState::new(&cnf(&[&[1, 2, 3], &[-1, 2]]));
        assert!(s.is_active(0) && s.is_active(1));

        s.enqueue(lit(1));
        assert!(s.is_satisfied(0), "clause 0 contains literal 1");
        assert!(s.is_active(1), "clause 1 contains -1, which is now false");
        assert!(s.counters_are_consistent());
    }

    #[test]
    fn unit_propagation_chains() {
        // 1 forces 2 forces 3.
        let mut s = SearchState::new(&cnf(&[&[1], &[-1, 2], &[-2, 3]]));
        assert!(s.propagate());
        assert_eq!(s.value(Var::from_index(0)), Some(true));
        assert_eq!(s.value(Var::from_index(1)), Some(true));
        assert_eq!(s.value(Var::from_index(2)), Some(true));
        assert!(s.counters_are_consistent());
    }

    #[test]
    fn contradictory_units_conflict() {
        let mut s = SearchState::new(&cnf(&[&[1], &[-1]]));
        assert!(!s.propagate());
        assert!(s.in_conflict());
    }

    #[test]
    fn empty_clause_conflicts_before_any_propagation() {
        let mut f = cnf(&[&[1]]);
        f.add_clause(&[]);
        let s = SearchState::new(&f);
        assert!(s.in_conflict());
    }

    #[test]
    fn backtracking_restores_counters_exactly() {
        let mut s = SearchState::new(&cnf(&[&[1, 2, 3], &[-1, -2], &[2, -3], &[-1, 3]]));
        let before: Vec<_> = (0..s.num_clauses())
            .map(|c| (s.sat_count[c], s.unassigned_count[c]))
            .collect();

        // Assigning 1 forces -2 and 3, and -2 then forces -3, which conflicts with 3. The point
        // here is not the verdict but that undoing it restores every counter exactly.
        s.push_level();
        s.enqueue(lit(1));
        assert!(!s.propagate(), "this branch conflicts");
        assert!(s.counters_are_consistent());
        s.pop_level();

        s.push_level();
        s.enqueue(lit(-1));
        assert!(s.propagate());
        assert!(s.counters_are_consistent());
        s.push_level();
        if s.value(Var::from_index(2)).is_none() {
            s.enqueue(lit(-3));
            s.propagate();
        }
        assert!(s.counters_are_consistent());

        s.pop_level();
        s.pop_level();

        let after: Vec<_> = (0..s.num_clauses())
            .map(|c| (s.sat_count[c], s.unassigned_count[c]))
            .collect();
        assert_eq!(
            before, after,
            "counters must return to their initial values"
        );
        assert_eq!(s.trail_len(), 0);
        assert!(!s.in_conflict());
        assert!(s.counters_are_consistent());
    }

    #[test]
    fn backtracking_clears_a_conflict() {
        let mut s = SearchState::new(&cnf(&[&[1, 2], &[-1, 2], &[1, -2], &[-1, -2]]));
        s.push_level();
        s.enqueue(lit(1));
        assert!(!s.propagate(), "1 forces 2 and -2");
        assert!(s.in_conflict());
        s.pop_level();
        assert!(!s.in_conflict());
        assert!(s.counters_are_consistent());
        assert_eq!(s.trail_len(), 0);
    }

    /// Textbook first-UIP: `a` and `b` each force `c` one way and the other. Resolving the
    /// conflict must produce a clause over the decisions, not over the implied literal.
    #[test]
    fn first_uip_resolves_back_to_the_decisions() {
        // Deciding a and then b forces c by the first clause and falsifies the second.
        let mut s = SearchState::new(&cnf(&[&[-1, -2, 3], &[-1, -2, -3]]));
        s.push_level();
        s.enqueue(lit(1));
        assert!(s.propagate(), "a alone settles nothing");
        s.push_level();
        s.enqueue(lit(2));
        assert!(!s.propagate(), "b forces c and -c");

        let Conflict::Learned {
            clause,
            assert_level,
            asserting,
        } = s.analyze_conflict(0)
        else {
            panic!("the conflict is not at the root");
        };
        let mut learned: Vec<i32> = s
            .clause(clause as usize)
            .iter()
            .map(|l| l.to_dimacs())
            .collect();
        learned.sort_unstable();
        assert_eq!(learned, vec![-2, -1], "resolving c away leaves (-a or -b)");
        assert_eq!(assert_level, 1, "-a sits on level 1");
        assert!(asserting, "exactly one literal is on the conflict level");
    }

    /// The derived clause must be unit, and the backjump level zero, when everything that caused
    /// the conflict was forced at the root.
    #[test]
    fn a_conflict_above_a_single_decision_yields_a_unit_clause() {
        let mut s = SearchState::new(&cnf(&[&[-1, 2], &[-1, -2]]));
        s.push_level();
        s.enqueue(lit(1));
        assert!(!s.propagate());

        let Conflict::Learned {
            clause,
            assert_level,
            asserting,
        } = s.analyze_conflict(0)
        else {
            panic!("the conflict is not at the root");
        };
        let learned: Vec<i32> = s
            .clause(clause as usize)
            .iter()
            .map(|l| l.to_dimacs())
            .collect();
        assert_eq!(learned, vec![-1]);
        assert_eq!(assert_level, 0);
        assert!(asserting);
    }

    /// A conflict that needs no decision at all is the formula's own; there is nothing to undo.
    #[test]
    fn a_root_conflict_is_reported_as_such() {
        let mut s = SearchState::new(&cnf(&[&[1], &[-1]]));
        assert!(!s.propagate());
        assert_eq!(s.analyze_conflict(0), Conflict::Root);
    }

    /// Pure literals carry no reason clause, so resolution has to stop at them and keep the
    /// literal. The clause is still a valid consequence, but it is not asserting, and the state
    /// has to say so rather than let the search backjump on it.
    #[test]
    fn a_conflict_through_a_pure_literal_is_not_asserting() {
        // Deciding a, then b, then assigning p as though pure: only all three together conflict,
        // and p has no reason clause to resolve against.
        let mut s = SearchState::new(&cnf(&[&[-1, -2, -4, 5], &[-1, -2, -4, -5]]));
        s.push_level();
        s.enqueue(lit(1));
        assert!(s.propagate());
        s.push_level();
        s.enqueue(lit(2));
        assert!(s.propagate());
        // `p` (variable 4) is assigned with no reason, on the same level as the decision `b`.
        s.enqueue(lit(4));
        assert!(!s.propagate(), "a, b and p together force e and -e");

        let Conflict::Learned {
            clause,
            assert_level,
            asserting,
        } = s.analyze_conflict(0)
        else {
            panic!("the conflict is not at the root");
        };
        let mut learned: Vec<i32> = s
            .clause(clause as usize)
            .iter()
            .map(|l| l.to_dimacs())
            .collect();
        learned.sort_unstable();
        assert_eq!(learned, vec![-4, -2, -1]);
        assert!(
            !asserting,
            "-b and -p are both on the conflict level, so nothing is forced by backjumping"
        );
        assert_eq!(assert_level, 2, "which is the conflict level itself");
    }

    /// Every derived clause is a resolvent of clauses already present, so it must be false under
    /// the assignment that produced it — that is what makes backjumping to `assert_level` safe.
    #[test]
    fn a_derived_clause_is_falsified_by_the_assignment_that_produced_it() {
        let mut s = SearchState::new(&cnf(&[
            &[-1, 2],
            &[-2, 3],
            &[-1, -3, 4],
            &[-4, -2],
            &[5, 1],
            &[-5, 1],
        ]));
        assert!(s.propagate(), "1 is forced at the root");
        s.push_level();
        s.enqueue(lit(-2));
        if s.propagate() {
            return; // Nothing to analyse; the shape of this formula changed.
        }
        let Conflict::Learned { clause, .. } = s.analyze_conflict(0) else {
            return;
        };

        for &l in s.clause(clause as usize) {
            assert_eq!(
                s.value(l.var()),
                Some(!l.is_positive()),
                "{l:?} of the derived clause should be false"
            );
        }
    }

    /// A derived clause longer than the caller will keep must be reported, not stored.
    #[test]
    fn an_over_long_derived_clause_is_discarded() {
        let mut s = SearchState::new(&cnf(&[&[-1, -2, 3], &[-1, -2, -3]]));
        s.push_level();
        s.enqueue(lit(1));
        assert!(s.propagate());
        s.push_level();
        s.enqueue(lit(2));
        assert!(!s.propagate());

        let before = s.num_clauses();
        assert_eq!(s.analyze_conflict(1), Conflict::TooLong(2));
        assert_eq!(s.num_clauses(), before, "nothing should have been added");
    }

    #[test]
    fn deep_assign_undo_cycles_stay_consistent() {
        // Hammer the incremental counters: a missed decrement anywhere shows up here.
        let mut s = SearchState::new(&cnf(&[
            &[1, 2, 3],
            &[-1, -2, 4],
            &[2, -3, -4],
            &[-2, 3, 4],
            &[1, -3, -4],
        ]));
        for mask in 0..16_u32 {
            for i in 0..4 {
                let sign = if mask >> i & 1 == 1 { 1 } else { -1 };
                s.push_level();
                let v = (i + 1) * sign;
                if s.value(Lit::from_dimacs(v).var()).is_none() {
                    s.enqueue(lit(v));
                    s.propagate();
                }
                assert!(s.counters_are_consistent(), "mask {mask} depth {i}");
            }
            for _ in 0..4 {
                s.pop_level();
            }
            assert_eq!(s.trail_len(), 0, "mask {mask}");
            assert!(s.counters_are_consistent());
        }
    }
}
