//! Splitting the residual formula into independent subproblems, and naming them canonically.
//!
//! This is where the dynamic programming actually happens. After some variables are assigned,
//! the clauses that remain active often fall into groups that share no variables. Those groups
//! can be solved independently, and — the part that pays — the *same* group tends to reappear
//! under many different decision orders, so a group solved once need never be solved again.
//!
//! # The key
//!
//! A component is identified by the pair
//!
//! ```text
//!     ( its unassigned variables , its active clauses )
//! ```
//!
//! both as sorted index sets. That pair determines the residual formula exactly. A clause is
//! active only if none of its literals is true, so every *assigned* variable occurring in an
//! active clause occurs falsely; the residual of clause `C` is therefore precisely
//! `{ l in C : var(l) is in the component }`, which the key already describes. The argument is
//! the one `Cachet` and `sharpSAT` rely on, and it is why the key can be two integer lists
//! rather than a canonicalised formula.
//!
//! Keys are stored delta-encoded as varints, which for the dense index sets a component
//! produces is close to one byte per element.
//!
//! The variable count is written *first* so that an entry describes the length of its own
//! satisfying witness; see [`crate::cache`].

use crate::lit::{Lit, Var};
use crate::search::state::SearchState;
use crate::varint;
use std::ops::Range;

/// A handle to a component held in a [`ComponentStore`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct ComponentRef(u32);

impl ComponentRef {
    /// Wraps a raw index, as returned in the range from [`ComponentAnalyzer::analyze`].
    #[inline]
    #[must_use]
    pub const fn new(index: usize) -> Self {
        Self(index as u32)
    }

    /// The underlying index.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Copy, Clone, Debug)]
struct Entry {
    var_start: u32,
    var_len: u32,
    clause_start: u32,
    clause_len: u32,
    key_start: u32,
    key_len: u32,
    hash: u64,
}

/// A stack of components, laid out in flat arenas.
///
/// Search is depth first, so components obey a stack discipline: everything a node pushes is
/// dropped when that node returns. [`ComponentStore::mark`] and [`ComponentStore::truncate`]
/// implement that, which keeps the whole structure to four `Vec`s that never shrink and so stop
/// allocating almost immediately.
#[derive(Debug, Default)]
pub struct ComponentStore {
    entries: Vec<Entry>,
    vars: Vec<u32>,
    clauses: Vec<u32>,
    keys: Vec<u8>,
}

/// A saved position in a [`ComponentStore`].
#[derive(Copy, Clone, Debug)]
pub struct StoreMark {
    entries: usize,
    vars: usize,
    clauses: usize,
    keys: usize,
}

impl ComponentStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the current position so it can be restored later.
    #[must_use]
    pub fn mark(&self) -> StoreMark {
        StoreMark {
            entries: self.entries.len(),
            vars: self.vars.len(),
            clauses: self.clauses.len(),
            keys: self.keys.len(),
        }
    }

    /// Discards everything pushed since `mark`.
    pub fn truncate(&mut self, mark: StoreMark) {
        self.entries.truncate(mark.entries);
        self.vars.truncate(mark.vars);
        self.clauses.truncate(mark.clauses);
        self.keys.truncate(mark.keys);
    }

    /// The unassigned variables of a component, ascending.
    #[inline]
    #[must_use]
    pub fn vars(&self, r: ComponentRef) -> &[u32] {
        let e = &self.entries[r.index()];
        &self.vars[e.var_start as usize..(e.var_start + e.var_len) as usize]
    }

    /// The active clauses of a component, ascending.
    #[inline]
    #[must_use]
    pub fn clauses(&self, r: ComponentRef) -> &[u32] {
        let e = &self.entries[r.index()];
        &self.clauses[e.clause_start as usize..(e.clause_start + e.clause_len) as usize]
    }

    /// The component's canonical key.
    #[inline]
    #[must_use]
    pub fn key(&self, r: ComponentRef) -> &[u8] {
        let e = &self.entries[r.index()];
        &self.keys[e.key_start as usize..(e.key_start + e.key_len) as usize]
    }

    /// The component key's 64-bit hash.
    #[inline]
    #[must_use]
    pub fn hash(&self, r: ComponentRef) -> u64 {
        self.entries[r.index()].hash
    }

    /// Bytes held by the store's arenas.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.entries.capacity() * size_of::<Entry>()
            + self.vars.capacity() * 4
            + self.clauses.capacity() * 4
            + self.keys.capacity()
    }

    /// Appends a component built from sorted variable and clause lists.
    fn push(&mut self, vars: &[u32], clauses: &[u32]) -> ComponentRef {
        let var_start = self.vars.len() as u32;
        self.vars.extend_from_slice(vars);
        let clause_start = self.clauses.len() as u32;
        self.clauses.extend_from_slice(clauses);

        let key_start = self.keys.len() as u32;
        varint::write_u32(&mut self.keys, vars.len() as u32);
        varint::write_u32(&mut self.keys, clauses.len() as u32);
        let mut previous = 0_u32;
        for &v in vars {
            varint::write_u32(&mut self.keys, v - previous);
            previous = v;
        }
        previous = 0;
        for &c in clauses {
            varint::write_u32(&mut self.keys, c - previous);
            previous = c;
        }
        let key_len = self.keys.len() as u32 - key_start;
        let hash = crate::cache::hash_key(&self.keys[key_start as usize..]);

        self.entries.push(Entry {
            var_start,
            var_len: vars.len() as u32,
            clause_start,
            clause_len: clauses.len() as u32,
            key_start,
            key_len,
            hash,
        });
        ComponentRef(self.entries.len() as u32 - 1)
    }
}

/// Scratch space for splitting a scope into components.
///
/// Reused across the whole solve. Visited marks are stamps rather than booleans, so a pass costs
/// nothing to start: bumping a counter invalidates every mark from the previous pass.
#[derive(Debug)]
pub struct ComponentAnalyzer {
    var_stamp: Vec<u32>,
    clause_stamp: Vec<u32>,
    stamp: u32,
    stack: Vec<u32>,
    scratch_vars: Vec<u32>,
    scratch_clauses: Vec<u32>,
    /// Active occurrences per literal within the pass, used for purity.
    lit_count: Vec<u32>,
    /// Literals found pure during the last [`ComponentAnalyzer::analyze`] call.
    pure: Vec<Lit>,
}

impl ComponentAnalyzer {
    /// Allocates scratch space for a formula of the given size.
    #[must_use]
    pub fn new(num_vars: usize, num_clauses: usize) -> Self {
        Self {
            var_stamp: vec![0; num_vars],
            clause_stamp: vec![0; num_clauses],
            stamp: 0,
            stack: Vec::new(),
            scratch_vars: Vec::new(),
            scratch_clauses: Vec::new(),
            lit_count: vec![0; 2 * num_vars],
            pure: Vec::new(),
        }
    }

    /// Literals found pure by the most recent analysis.
    ///
    /// A literal is pure when its complement occurs in no active clause. Assigning it cannot
    /// falsify anything, so it can never cause a conflict or a propagation — which is why the
    /// caller may simply assign the whole batch and re-analyse.
    ///
    /// Pure literal elimination preserves satisfiability but *not* the number of models, so this
    /// is sound here and would not be in a model counter.
    #[inline]
    #[must_use]
    pub fn pure_literals(&self) -> &[Lit] {
        &self.pure
    }

    /// Splits `scope` into connected components, appending them to `store`.
    ///
    /// `scope` lists candidate variables; assigned ones are skipped. Variables whose clauses are
    /// all satisfied are dropped rather than becoming singleton components — they are free, need
    /// no search, and would only pollute the memo.
    ///
    /// Returns the range of newly created components. When `detect_pure` is set, purity is
    /// computed as a side effect of the same scan; see [`Self::pure_literals`].
    pub fn analyze(
        &mut self,
        state: &SearchState,
        scope: &[u32],
        store: &mut ComponentStore,
        detect_pure: bool,
    ) -> Range<usize> {
        self.bump_stamp();
        self.pure.clear();
        let first = store.entries.len();

        for &seed in scope {
            if state.value_of_index(seed as usize).is_some()
                || self.var_stamp[seed as usize] == self.stamp
            {
                continue;
            }
            self.scratch_vars.clear();
            self.scratch_clauses.clear();
            self.stack.clear();

            self.var_stamp[seed as usize] = self.stamp;
            self.stack.push(seed);

            while let Some(v) = self.stack.pop() {
                self.scratch_vars.push(v);
                let var = Var::from_index(v as usize);
                for lit in [var.positive(), var.negative()] {
                    for &c in state.occurrences(lit) {
                        let c = c as usize;
                        if !state.is_active(c) || self.clause_stamp[c] == self.stamp {
                            continue;
                        }
                        self.clause_stamp[c] = self.stamp;
                        self.scratch_clauses.push(c as u32);
                        for &l in state.clause(c) {
                            let w = l.var().index();
                            if state.value_of_index(w).is_some() {
                                continue;
                            }
                            if detect_pure {
                                self.lit_count[l.index()] += 1;
                            }
                            if self.var_stamp[w] != self.stamp {
                                self.var_stamp[w] = self.stamp;
                                self.stack.push(w as u32);
                            }
                        }
                    }
                }
            }

            if self.scratch_clauses.is_empty() {
                // A free variable: every clause it occurs in is already satisfied.
                continue;
            }

            self.scratch_vars.sort_unstable();
            self.scratch_clauses.sort_unstable();

            if detect_pure {
                for &v in &self.scratch_vars {
                    let var = Var::from_index(v as usize);
                    let pos = self.lit_count[var.positive().index()];
                    let neg = self.lit_count[var.negative().index()];
                    if pos > 0 && neg == 0 {
                        self.pure.push(var.positive());
                    } else if neg > 0 && pos == 0 {
                        self.pure.push(var.negative());
                    }
                    self.lit_count[var.positive().index()] = 0;
                    self.lit_count[var.negative().index()] = 0;
                }
            }

            store.push(&self.scratch_vars, &self.scratch_clauses);
        }

        first..store.entries.len()
    }

    /// Collects `scope` into a single component without splitting it.
    ///
    /// Used by the plain-DPLL ablation, so that "with decomposition" and "without" differ in one
    /// thing only and share every other line of the search.
    pub fn analyze_whole(
        &mut self,
        state: &SearchState,
        scope: &[u32],
        store: &mut ComponentStore,
        detect_pure: bool,
    ) -> Range<usize> {
        self.bump_stamp();
        self.pure.clear();
        let first = store.entries.len();
        self.scratch_vars.clear();
        self.scratch_clauses.clear();

        for &v in scope {
            if state.value_of_index(v as usize).is_some() {
                continue;
            }
            let var = Var::from_index(v as usize);
            let mut occurs = false;
            for lit in [var.positive(), var.negative()] {
                for &c in state.occurrences(lit) {
                    let c = c as usize;
                    if !state.is_active(c) {
                        continue;
                    }
                    occurs = true;
                    if self.clause_stamp[c] != self.stamp {
                        self.clause_stamp[c] = self.stamp;
                        self.scratch_clauses.push(c as u32);
                        if detect_pure {
                            for &l in state.clause(c) {
                                if state.value_of_index(l.var().index()).is_none() {
                                    self.lit_count[l.index()] += 1;
                                }
                            }
                        }
                    }
                }
            }
            if occurs {
                self.scratch_vars.push(v);
            }
        }

        if self.scratch_clauses.is_empty() {
            return first..first;
        }
        self.scratch_vars.sort_unstable();
        self.scratch_clauses.sort_unstable();

        if detect_pure {
            for &v in &self.scratch_vars {
                let var = Var::from_index(v as usize);
                let pos = self.lit_count[var.positive().index()];
                let neg = self.lit_count[var.negative().index()];
                if pos > 0 && neg == 0 {
                    self.pure.push(var.positive());
                } else if neg > 0 && pos == 0 {
                    self.pure.push(var.negative());
                }
                self.lit_count[var.positive().index()] = 0;
                self.lit_count[var.negative().index()] = 0;
            }
        }

        store.push(&self.scratch_vars, &self.scratch_clauses);
        first..store.entries.len()
    }

    /// Advances the visited stamp, clearing the marks wholesale on wrap-around.
    fn bump_stamp(&mut self) {
        if self.stamp == u32::MAX {
            self.var_stamp.fill(0);
            self.clause_stamp.fill(0);
            self.stamp = 0;
        }
        self.stamp += 1;
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

    fn setup(f: &Cnf) -> (SearchState, ComponentAnalyzer, ComponentStore) {
        (
            SearchState::new(f),
            ComponentAnalyzer::new(f.num_vars(), f.num_clauses()),
            ComponentStore::new(),
        )
    }

    fn all_vars(n: usize) -> Vec<u32> {
        (0..n as u32).collect()
    }

    #[test]
    fn a_connected_formula_is_one_component() {
        let f = cnf(&[&[1, 2], &[2, 3], &[3, 1]]);
        let (state, mut a, mut store) = setup(&f);
        let range = a.analyze(&state, &all_vars(3), &mut store, false);
        assert_eq!(range.len(), 1);
        let c = ComponentRef(range.start as u32);
        assert_eq!(store.vars(c), &[0, 1, 2]);
        assert_eq!(store.clauses(c), &[0, 1, 2]);
    }

    #[test]
    fn disjoint_clause_groups_split() {
        // Two independent 2-colourings that share no variables at all.
        let f = cnf(&[&[1, 2], &[-1, -2], &[3, 4], &[-3, -4]]);
        let (state, mut a, mut store) = setup(&f);
        let range = a.analyze(&state, &all_vars(4), &mut store, false);
        assert_eq!(range.len(), 2);
        let first = ComponentRef(range.start as u32);
        let second = ComponentRef(range.start as u32 + 1);
        assert_eq!(store.vars(first), &[0, 1]);
        assert_eq!(store.clauses(first), &[0, 1]);
        assert_eq!(store.vars(second), &[2, 3]);
        assert_eq!(store.clauses(second), &[2, 3]);
        assert_ne!(store.hash(first), store.hash(second));
    }

    /// The behaviour the whole design rests on: assigning a variable can break one component
    /// into several, which is what creates reusable subproblems.
    #[test]
    fn assignment_splits_a_component_in_two() {
        // Variable 1 is the only link between the two halves.
        let f = cnf(&[&[1, 2], &[2, 3], &[1, 4], &[4, 5]]);
        let (mut state, mut a, mut store) = setup(&f);

        let whole = a.analyze(&state, &all_vars(5), &mut store, false);
        assert_eq!(whole.len(), 1, "connected before assigning");
        store.truncate(store.mark());

        state.push_level();
        state.enqueue(Lit::from_dimacs(1));
        assert!(state.propagate());

        let split = a.analyze(&state, &all_vars(5), &mut store, false);
        // Clauses 0 and 2 are satisfied by 1; what remains is {2,3} and {4,5}, disjoint.
        assert_eq!(split.len(), 2);
        let l = ComponentRef(split.start as u32);
        let r = ComponentRef(split.start as u32 + 1);
        assert_eq!(store.vars(l), &[1, 2]);
        assert_eq!(store.vars(r), &[3, 4]);
    }

    #[test]
    fn satisfied_clauses_and_free_variables_are_excluded() {
        let f = cnf(&[&[1], &[2, 3]]);
        let (mut state, mut a, mut store) = setup(&f);
        assert!(state.propagate(), "the unit clause assigns variable 1");

        let range = a.analyze(&state, &all_vars(3), &mut store, false);
        assert_eq!(range.len(), 1);
        let c = ComponentRef(range.start as u32);
        assert_eq!(
            store.vars(c),
            &[1, 2],
            "variable 1 is assigned, so it is gone"
        );
        assert_eq!(
            store.clauses(c),
            &[1],
            "clause 0 is satisfied, so it is gone"
        );
    }

    #[test]
    fn a_variable_with_only_satisfied_clauses_is_not_a_component() {
        let f = cnf(&[&[1, 2]]);
        let (mut state, mut a, mut store) = setup(&f);
        state.enqueue(Lit::from_dimacs(1));
        // Variable 2 is now free: its only clause is satisfied.
        let range = a.analyze(&state, &all_vars(2), &mut store, false);
        assert_eq!(range.len(), 0);
    }

    #[test]
    fn identical_subproblems_reached_differently_get_identical_keys() {
        // This is the property that makes the memo hit at all: assigning 1 then 2, or 2 then 1,
        // must produce the same name for the subproblem that survives.
        let f = cnf(&[&[1, 5], &[2, 5], &[3, 4], &[-3, 4]]);
        let (mut state, mut a, mut store) = setup(&f);

        state.push_level();
        state.enqueue(Lit::from_dimacs(1));
        state.enqueue(Lit::from_dimacs(2));
        state.propagate();
        let first = a.analyze(&state, &all_vars(5), &mut store, false);
        let key_a = store.key(ComponentRef(first.start as u32)).to_vec();
        let hash_a = store.hash(ComponentRef(first.start as u32));
        state.pop_level();

        state.push_level();
        state.enqueue(Lit::from_dimacs(2));
        state.enqueue(Lit::from_dimacs(1));
        state.propagate();
        let second = a.analyze(&state, &all_vars(5), &mut store, false);
        let key_b = store.key(ComponentRef(second.start as u32)).to_vec();
        let hash_b = store.hash(ComponentRef(second.start as u32));
        state.pop_level();

        assert_eq!(key_a, key_b, "same residual formula must get the same key");
        assert_eq!(hash_a, hash_b);
    }

    #[test]
    fn different_subproblems_get_different_keys() {
        // Same clause set, different unassigned variables: the keys must differ, which is why
        // the key records both halves rather than just the clauses.
        let f = cnf(&[&[1, 2, 3], &[-1, 2, 3]]);
        let (mut state, mut a, mut store) = setup(&f);

        let whole = a.analyze(&state, &all_vars(3), &mut store, false);
        let key_a = store.key(ComponentRef(whole.start as u32)).to_vec();

        state.push_level();
        state.enqueue(Lit::from_dimacs(-1));
        state.propagate();
        let part = a.analyze(&state, &all_vars(3), &mut store, false);
        // Clause 1 is satisfied by -1; clause 0 survives with variable 1 assigned.
        let key_b = store.key(ComponentRef(part.start as u32)).to_vec();
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn pure_literals_are_detected() {
        // Variable 3 only ever occurs positively among active clauses.
        let f = cnf(&[&[1, 3], &[-1, 3], &[1, -2], &[-1, 2]]);
        let (state, mut a, mut store) = setup(&f);
        a.analyze(&state, &all_vars(3), &mut store, true);
        assert_eq!(a.pure_literals(), &[Lit::from_dimacs(3)]);
    }

    #[test]
    fn a_variable_on_both_sides_is_not_pure() {
        let f = cnf(&[&[1, 2], &[-1, 2], &[-2, 1]]);
        let (state, mut a, mut store) = setup(&f);
        a.analyze(&state, &all_vars(2), &mut store, true);
        assert!(a.pure_literals().is_empty());
    }

    #[test]
    fn purity_counts_only_active_clauses() {
        // -3 occurs, but only in a clause that is already satisfied, so 3 is pure after all.
        let f = cnf(&[&[1, 3], &[-3, 2]]);
        let (mut state, mut a, mut store) = setup(&f);
        state.enqueue(Lit::from_dimacs(2));
        a.analyze(&state, &all_vars(3), &mut store, true);
        assert!(a.pure_literals().contains(&Lit::from_dimacs(3)));
    }

    #[test]
    fn store_truncation_returns_to_the_mark() {
        let f = cnf(&[&[1, 2], &[3, 4]]);
        let (state, mut a, mut store) = setup(&f);
        let mark = store.mark();
        let range = a.analyze(&state, &all_vars(4), &mut store, false);
        assert_eq!(range.len(), 2);
        store.truncate(mark);
        let again = a.analyze(&state, &all_vars(4), &mut store, false);
        assert_eq!(again, 0..2, "the store should reuse the reclaimed slots");
    }

    #[test]
    fn keys_are_compact() {
        // 40 clauses over 20 variables should cost roughly a byte per element, not four.
        let clauses: Vec<Vec<i32>> = (0..40)
            .map(|i| vec![(i % 20) + 1, ((i + 3) % 20) + 1, -(((i + 7) % 20) + 1)])
            .collect();
        let refs: Vec<&[i32]> = clauses.iter().map(std::vec::Vec::as_slice).collect();
        let f = cnf(&refs);
        let (state, mut a, mut store) = setup(&f);
        let range = a.analyze(&state, &all_vars(f.num_vars()), &mut store, false);
        let key = store.key(ComponentRef(range.start as u32));
        assert!(
            key.len() <= 64,
            "key of {} bytes for 20 vars + 40 clauses",
            key.len()
        );
    }
}
