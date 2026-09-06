//! The mutable search state: assignment, trail, and per-clause counters.
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

use crate::cnf::Cnf;
use crate::lit::{Lit, Var};

/// Assignment, trail, and clause counters for one solve.
#[derive(Debug)]
pub struct SearchState {
    num_vars: usize,

    // --- static formula, flattened -----------------------------------------------------------
    lits: Vec<Lit>,
    /// `clause_bounds[c]..clause_bounds[c + 1]` is clause `c`.
    clause_bounds: Vec<u32>,
    /// `occ_bounds[l]..occ_bounds[l + 1]` indexes `occ`, giving the clauses containing literal `l`.
    occ_bounds: Vec<u32>,
    occ: Vec<u32>,

    // --- dynamic state -----------------------------------------------------------------------
    /// True literals per clause. Zero means the clause is still active.
    sat_count: Vec<u32>,
    /// Unassigned literals per clause.
    unassigned_count: Vec<u32>,
    value: Vec<Option<bool>>,
    trail: Vec<Lit>,
    trail_lim: Vec<usize>,
    /// Clauses observed to have become unit, to be drained by [`SearchState::propagate`].
    unit_queue: Vec<u32>,
    conflict: bool,

    // --- counters ----------------------------------------------------------------------------
    propagations: u64,
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
            lits,
            clause_bounds,
            occ_bounds,
            occ,
            sat_count: vec![0; num_clauses],
            unassigned_count,
            value: vec![None; num_vars],
            trail: Vec::with_capacity(num_vars),
            trail_lim: Vec::new(),
            unit_queue: Vec::new(),
            conflict: false,
            propagations: 0,
        };

        for c in 0..num_clauses {
            match state.unassigned_count[c] {
                0 => state.conflict = true,
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

    /// Number of clauses.
    #[inline]
    #[must_use]
    pub fn num_clauses(&self) -> usize {
        self.clause_bounds.len() - 1
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

    /// The clauses containing literal `l`.
    #[inline]
    #[must_use]
    pub fn occurrences(&self, l: Lit) -> &[u32] {
        let start = self.occ_bounds[l.index()] as usize;
        let end = self.occ_bounds[l.index() + 1] as usize;
        &self.occ[start..end]
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

    /// Assigns `lit` to true and updates every clause it occurs in.
    ///
    /// Clauses that become unit are queued; a clause that becomes empty sets the conflict flag.
    /// Counters are always brought fully up to date even when a conflict is detected part way,
    /// so that [`SearchState::pop_level`] can undo the assignment exactly.
    ///
    /// # Panics
    /// Panics in debug builds if the variable is already assigned.
    pub fn enqueue(&mut self, lit: Lit) {
        debug_assert!(self.value(lit.var()).is_none(), "{lit:?} is already assigned");
        self.value[lit.var().index()] = Some(lit.is_positive());
        self.trail.push(lit);
        self.propagations += 1;

        // Clauses this literal satisfies.
        let (start, end) = self.occ_range(lit);
        for k in start..end {
            let c = self.occ[k] as usize;
            self.sat_count[c] += 1;
            self.unassigned_count[c] -= 1;
        }

        // Clauses this literal shortens.
        let (start, end) = self.occ_range(!lit);
        for k in start..end {
            let c = self.occ[k] as usize;
            self.unassigned_count[c] -= 1;
            if self.sat_count[c] == 0 {
                match self.unassigned_count[c] {
                    0 => self.conflict = true,
                    1 => self.unit_queue.push(c as u32),
                    _ => {}
                }
            }
        }
    }

    #[inline]
    fn occ_range(&self, l: Lit) -> (usize, usize) {
        (self.occ_bounds[l.index()] as usize, self.occ_bounds[l.index() + 1] as usize)
    }

    /// Runs unit propagation to fixpoint.
    ///
    /// Returns `false` on conflict, in which case the caller must [`SearchState::pop_level`]
    /// before doing anything else.
    pub fn propagate(&mut self) -> bool {
        while !self.conflict {
            let Some(c) = self.unit_queue.pop() else { break };
            let c = c as usize;
            // The queue records clauses that *were* unit; by the time one is drained it may have
            // been satisfied or shortened further, so re-check rather than trust the entry.
            if self.sat_count[c] > 0 {
                continue;
            }
            if self.unassigned_count[c] == 0 {
                self.conflict = true;
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
            self.enqueue(implied);
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
        let target = self.trail_lim.pop().expect("pop_level without a matching push_level");
        self.unassign_to(target);
        self.conflict = false;
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
            let (start, end) = self.occ_range(!lit);
            for k in start..end {
                let c = self.occ[k] as usize;
                self.unassigned_count[c] += 1;
            }
            self.value[lit.var().index()] = None;
        }
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
            let sat = clause.iter().filter(|l| self.value(l.var()) == Some(l.is_positive())).count();
            let free = clause.iter().filter(|l| self.value(l.var()).is_none()).count();
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

    #[test]
    fn occurrence_lists_are_complete() {
        let s = SearchState::new(&cnf(&[&[1, 2], &[-1, 3], &[1, -3]]));
        assert_eq!(s.occurrences(lit(1)), &[0, 2]);
        assert_eq!(s.occurrences(lit(-1)), &[1]);
        assert_eq!(s.occurrences(lit(3)), &[1]);
        assert_eq!(s.occurrences(lit(-3)), &[2]);
        assert_eq!(s.occurrences(lit(-2)), &[] as &[u32]);
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
        let before: Vec<_> = (0..s.num_clauses()).map(|c| (s.sat_count[c], s.unassigned_count[c])).collect();

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

        let after: Vec<_> = (0..s.num_clauses()).map(|c| (s.sat_count[c], s.unassigned_count[c])).collect();
        assert_eq!(before, after, "counters must return to their initial values");
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
