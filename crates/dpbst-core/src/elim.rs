//! Variable elimination by resolution, and the bookkeeping that lets a model survive it.
//!
//! This module is the *other* DP: Davis and Putnam's 1960 procedure, which eliminates a variable
//! by replacing every clause mentioning it with the resolvents of its positive and negative
//! occurrences. Run to completion it decides satisfiability; run under a size limit it is
//! **bounded variable elimination**, the preprocessing step that modern solvers kept.
//!
//! Both uses share this code — [`crate::preprocess`] applies it with a growth limit,
//! [`crate::dp`] without one.
//!
//! # Keeping the model
//!
//! Eliminating a variable throws away the information needed to assign it. Every elimination
//! therefore pushes the clauses it removed onto a [`Reconstruction`] trail, replayed in reverse
//! once the search has a model for what remains. This is the technique from Eén and Biere's
//! *Effective Preprocessing in SAT through Variable and Clause Elimination*.

use crate::cnf::{Cnf, Model};
use crate::lit::{Lit, Var};

/// A mutable clause store with occurrence lists, used during elimination.
///
/// Deletion is by tombstone: clauses become `None` and occurrence lists keep stale indices,
/// which readers skip. Compaction happens once, when the formula is handed back.
#[derive(Debug)]
pub struct Working {
    clauses: Vec<Option<Vec<Lit>>>,
    /// Clause indices per literal. May name deleted clauses or clauses no longer containing the
    /// literal; both are filtered on read.
    occ: Vec<Vec<usize>>,
    num_vars: usize,
    live: usize,
}

impl Working {
    /// Builds a working store from a normalised formula.
    #[must_use]
    pub fn new(cnf: &Cnf) -> Self {
        let num_vars = cnf.num_vars();
        let mut w = Self {
            clauses: Vec::with_capacity(cnf.num_clauses()),
            occ: vec![Vec::new(); 2 * num_vars],
            num_vars,
            live: 0,
        };
        for clause in cnf.clauses() {
            w.add(clause.to_vec());
        }
        w
    }

    /// Number of variables.
    #[inline]
    #[must_use]
    pub const fn num_vars(&self) -> usize {
        self.num_vars
    }

    /// Number of clauses still present.
    #[inline]
    #[must_use]
    pub const fn live_clauses(&self) -> usize {
        self.live
    }

    /// Whether every clause has been removed, which makes the formula satisfiable.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Adds a clause, returning its index.
    pub fn add(&mut self, clause: Vec<Lit>) -> usize {
        let index = self.clauses.len();
        for &l in &clause {
            self.occ[l.index()].push(index);
        }
        self.clauses.push(Some(clause));
        self.live += 1;
        index
    }

    /// Removes a clause.
    pub fn remove(&mut self, index: usize) {
        if self.clauses[index].take().is_some() {
            self.live -= 1;
        }
    }

    /// Borrows a clause, if it is still present.
    #[inline]
    #[must_use]
    pub fn clause(&self, index: usize) -> Option<&[Lit]> {
        self.clauses[index].as_deref()
    }

    /// Live clause indices containing `lit`.
    ///
    /// Filters the occurrence list, which may hold stale entries.
    #[must_use]
    pub fn occurrences(&self, lit: Lit) -> Vec<usize> {
        self.occ[lit.index()]
            .iter()
            .copied()
            .filter(|&c| self.clauses[c].as_ref().is_some_and(|cl| cl.contains(&lit)))
            .collect()
    }

    /// Number of live clauses containing `lit`.
    ///
    /// Deliberately does not build the list: this runs once per variable per elimination round,
    /// and allocating there dominated the whole procedure.
    #[must_use]
    pub fn count(&self, lit: Lit) -> usize {
        self.occ[lit.index()]
            .iter()
            .filter(|&&c| self.clauses[c].as_ref().is_some_and(|cl| cl.contains(&lit)))
            .count()
    }

    /// Tally of live occurrences for every literal, in one pass over the formula.
    ///
    /// Indexed by [`Lit::index`]. Computing all of them together is `O(total literals)`, where
    /// asking per literal is `O(variables * total literals)`.
    #[must_use]
    pub fn literal_counts(&self) -> Vec<u32> {
        let mut counts = vec![0_u32; 2 * self.num_vars];
        for clause in self.clauses.iter().flatten() {
            for &l in clause {
                counts[l.index()] += 1;
            }
        }
        counts
    }

    /// Iterates over the indices of live clauses.
    pub fn live_indices(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.clauses.len()).filter(move |&i| self.clauses[i].is_some())
    }

    /// Whether the empty clause is present, which makes the formula unsatisfiable.
    #[must_use]
    pub fn has_empty_clause(&self) -> bool {
        self.clauses.iter().any(|c| c.as_ref().is_some_and(std::vec::Vec::is_empty))
    }

    /// Rebuilds a compact [`Cnf`] from the surviving clauses.
    #[must_use]
    pub fn to_cnf(&self) -> Cnf {
        let mut out = Cnf::new(self.num_vars);
        for i in self.live_indices() {
            out.add_clause(self.clauses[i].as_ref().expect("live index"));
        }
        out
    }

    /// Ratio of occurrence-list entries that no longer name a live clause.
    ///
    /// Compaction is `O(total occurrences * clause length)`, so it is worth doing only once the
    /// lists are mostly rubbish.
    #[must_use]
    pub fn tombstone_ratio(&self) -> f64 {
        let total: usize = self.occ.iter().map(Vec::len).sum();
        if total == 0 {
            return 0.0;
        }
        let dead: usize =
            self.occ.iter().flatten().filter(|&&c| self.clauses[c].is_none()).count();
        dead as f64 / total as f64
    }

    /// Drops the occurrence lists' stale entries, bounding their growth over a long run.
    pub fn compact_occurrences(&mut self) {
        for lit_index in 0..self.occ.len() {
            let lit = Lit::from_index(lit_index);
            let clauses = &self.clauses;
            self.occ[lit_index]
                .retain(|&c| clauses[c].as_ref().is_some_and(|cl| cl.contains(&lit)));
        }
    }
}

/// Resolves two clauses on `var`.
///
/// Returns `None` when the resolvent is a tautology, which is the case that keeps Davis–Putnam
/// from producing even more clauses than it already does. The result is sorted and duplicate-free
/// so that it can be compared and subsumed directly.
#[must_use]
pub fn resolve(a: &[Lit], b: &[Lit], var: Var) -> Option<Vec<Lit>> {
    let mut out: Vec<Lit> = Vec::with_capacity(a.len() + b.len() - 2);
    out.extend(a.iter().copied().filter(|l| l.var() != var));
    out.extend(b.iter().copied().filter(|l| l.var() != var));
    out.sort_unstable();
    out.dedup();
    // Complementary literals are adjacent after sorting: they differ only in bit 0.
    if out.windows(2).any(|w| w[0] == !w[1]) { None } else { Some(out) }
}

/// One undoable simplification.
#[derive(Debug, Clone)]
enum Step {
    /// A variable was forced, by a unit clause or by being pure.
    Fixed(Lit),
    /// A variable was eliminated by resolution; the listed clauses were removed with it.
    Eliminated { var: Var, clauses: Vec<Vec<Lit>> },
}

/// The trail that turns a model of the simplified formula into a model of the original.
#[derive(Debug, Default, Clone)]
pub struct Reconstruction {
    steps: Vec<Step>,
}

impl Reconstruction {
    /// Creates an empty trail.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `lit` was forced true.
    pub fn fix(&mut self, lit: Lit) {
        self.steps.push(Step::Fixed(lit));
    }

    /// Records that `var` was eliminated, along with every clause that mentioned it.
    pub fn eliminate(&mut self, var: Var, clauses: Vec<Vec<Lit>>) {
        self.steps.push(Step::Eliminated { var, clauses });
    }

    /// Number of recorded steps.
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether anything was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Extends a model of the simplified formula to the original.
    ///
    /// Steps are replayed in reverse. A forced variable takes its forced value. An eliminated
    /// variable is given whichever polarity satisfies every clause that was removed with it; at
    /// least one polarity always does, because the resolvents that replaced those clauses are
    /// satisfied by the model already in hand.
    ///
    /// # Panics
    /// Panics if neither polarity works, which would mean the elimination was unsound.
    pub fn extend(&self, model: &mut Model) {
        for step in self.steps.iter().rev() {
            match step {
                Step::Fixed(lit) => model.set(lit.var(), lit.is_positive()),
                Step::Eliminated { var, clauses } => {
                    model.set(*var, false);
                    let satisfied = |m: &Model| {
                        clauses
                            .iter()
                            .all(|c| c.iter().any(|&l| m.value(l.var()) == l.is_positive()))
                    };
                    if !satisfied(model) {
                        model.set(*var, true);
                        assert!(
                            satisfied(model),
                            "neither polarity of {var:?} satisfies its eliminated clauses; \
                             variable elimination was unsound"
                        );
                    }
                }
            }
        }
    }
}

/// Propagates unit clauses to fixpoint.
///
/// Returns `false` if a conflict is derived, which makes the formula unsatisfiable.
pub fn unit_propagate(w: &mut Working, trail: &mut Reconstruction) -> bool {
    // A worklist, seeded once. Rescanning the whole formula after every implication makes this
    // quadratic in the clause count, which Davis-Putnam reaches quickly.
    let mut queue: Vec<Lit> = w
        .live_indices()
        .filter_map(|i| match w.clause(i) {
            Some([l]) => Some(*l),
            _ => None,
        })
        .collect();
    let mut value: Vec<Option<bool>> = vec![None; w.num_vars()];

    while let Some(lit) = queue.pop() {
        match value[lit.var().index()] {
            Some(assigned) if assigned == lit.is_positive() => continue,
            // Both polarities were derived as units: the formula is unsatisfiable.
            Some(_) => return false,
            None => value[lit.var().index()] = Some(lit.is_positive()),
        }

        trail.fix(lit);
        for c in w.occurrences(lit) {
            w.remove(c);
        }
        // Strengthen the clauses the literal falsifies.
        for c in w.occurrences(!lit) {
            let mut clause = w.clause(c).expect("live index").to_vec();
            clause.retain(|&l| l != !lit);
            w.remove(c);
            match clause.as_slice() {
                [] => return false,
                [single] => queue.push(*single),
                _ => {}
            }
            w.add(clause);
        }
    }
    !w.has_empty_clause()
}

/// Assigns every pure literal.
///
/// Sound for satisfiability — a literal whose complement never occurs can always be set true —
/// but not for model counting, which is why this is a SAT solver and not a `#SAT` solver.
/// Returns the number of variables fixed.
pub fn eliminate_pure_literals(w: &mut Working, trail: &mut Reconstruction) -> usize {
    let mut fixed = 0;
    loop {
        // One counting pass finds every pure literal at once, and removing them can only create
        // more, so the round repeats until a pass finds none.
        let counts = w.literal_counts();
        let mut pure = Vec::new();
        for v in 0..w.num_vars() {
            let var = Var::from_index(v);
            let pos = counts[var.positive().index()];
            let neg = counts[var.negative().index()];
            if pos > 0 && neg == 0 {
                pure.push(var.positive());
            } else if neg > 0 && pos == 0 {
                pure.push(var.negative());
            }
        }
        if pure.is_empty() {
            return fixed;
        }
        for lit in pure {
            trail.fix(lit);
            for c in w.occurrences(lit) {
                w.remove(c);
            }
            fixed += 1;
        }
    }
}

/// Eliminates `var` by resolution, unconditionally.
///
/// Every clause containing the variable is replaced by the non-tautological resolvents. Returns
/// the number of resolvents added.
pub fn eliminate_var(w: &mut Working, var: Var, trail: &mut Reconstruction) -> usize {
    let positives = w.occurrences(var.positive());
    let negatives = w.occurrences(var.negative());

    let mut removed = Vec::with_capacity(positives.len() + negatives.len());
    let mut resolvents = Vec::new();
    for &p in &positives {
        for &n in &negatives {
            let a = w.clause(p).expect("live index");
            let b = w.clause(n).expect("live index");
            if let Some(r) = resolve(a, b, var) {
                resolvents.push(r);
            }
        }
    }
    for &c in positives.iter().chain(negatives.iter()) {
        removed.push(w.clause(c).expect("live index").to_vec());
        w.remove(c);
    }

    let added = resolvents.len();
    for r in resolvents {
        w.add(r);
    }
    trail.eliminate(var, removed);
    added
}

/// Counts the non-tautological resolvents eliminating `var` would produce, without doing it.
///
/// This is the test bounded variable elimination gates on.
#[must_use]
pub fn resolvent_count(w: &Working, var: Var) -> usize {
    let positives = w.occurrences(var.positive());
    let negatives = w.occurrences(var.negative());
    let mut count = 0;
    for &p in &positives {
        for &n in &negatives {
            let a = w.clause(p).expect("live index");
            let b = w.clause(n).expect("live index");
            if resolve(a, b, var).is_some() {
                count += 1;
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cnf(clauses: &[&[i32]]) -> Cnf {
        let mut f = Cnf::new(0);
        for c in clauses {
            f.add_dimacs_clause(c);
        }
        f.normalized().0
    }

    fn lit(n: i32) -> Lit {
        Lit::from_dimacs(n)
    }

    fn var(n: i32) -> Var {
        Var::from_index(n as usize - 1)
    }

    #[test]
    fn resolution_removes_the_pivot() {
        let a = [lit(1), lit(2)];
        let b = [lit(-1), lit(3)];
        let r = resolve(&a, &b, var(1)).expect("not a tautology");
        assert_eq!(r, vec![lit(2), lit(3)]);
    }

    #[test]
    fn tautological_resolvents_are_dropped() {
        // (1 or 2) resolved with (-1 or -2) gives (2 or -2).
        assert!(resolve(&[lit(1), lit(2)], &[lit(-1), lit(-2)], var(1)).is_none());
    }

    #[test]
    fn resolution_deduplicates() {
        let r = resolve(&[lit(1), lit(2)], &[lit(-1), lit(2), lit(3)], var(1)).unwrap();
        assert_eq!(r, vec![lit(2), lit(3)]);
    }

    #[test]
    fn occurrence_lists_skip_removed_clauses() {
        let mut w = Working::new(&cnf(&[&[1, 2], &[1, 3], &[-1, 4]]));
        assert_eq!(w.count(lit(1)), 2);
        w.remove(0);
        assert_eq!(w.count(lit(1)), 1);
        assert_eq!(w.live_clauses(), 2);
    }

    #[test]
    fn unit_propagation_fixes_and_strengthens() {
        let mut w = Working::new(&cnf(&[&[1], &[-1, 2], &[2, 3]]));
        let mut t = Reconstruction::new();
        assert!(unit_propagate(&mut w, &mut t));
        // 1 is true, so (-1 or 2) becomes the unit (2), which then satisfies (2 or 3).
        assert!(w.is_empty());
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn unit_propagation_detects_conflict() {
        let mut w = Working::new(&cnf(&[&[1], &[-1]]));
        let mut t = Reconstruction::new();
        assert!(!unit_propagate(&mut w, &mut t));
    }

    #[test]
    fn pure_literals_are_removed() {
        let mut w = Working::new(&cnf(&[&[1, 2], &[1, -2], &[-2, 3]]));
        let mut t = Reconstruction::new();
        // 1 is pure positive; after its clauses go, 3 is pure and -2 is pure.
        assert!(eliminate_pure_literals(&mut w, &mut t) >= 1);
        assert!(w.is_empty());
    }

    #[test]
    fn eliminating_a_variable_replaces_its_clauses() {
        // (1 or 2), (-1 or 3) resolve to (2 or 3); both originals go away.
        let mut w = Working::new(&cnf(&[&[1, 2], &[-1, 3], &[4, 5]]));
        let mut t = Reconstruction::new();
        assert_eq!(resolvent_count(&w, var(1)), 1);
        assert_eq!(eliminate_var(&mut w, var(1), &mut t), 1);
        assert_eq!(w.live_clauses(), 2);
        assert_eq!(w.count(lit(1)), 0);
        assert_eq!(w.count(lit(-1)), 0);
    }

    #[test]
    fn reconstruction_recovers_a_fixed_variable() {
        let mut t = Reconstruction::new();
        t.fix(lit(-2));
        let mut m = Model::all_false(3);
        m.set(var(2), true);
        t.extend(&mut m);
        assert!(!m.value(var(2)));
    }

    #[test]
    fn reconstruction_recovers_an_eliminated_variable() {
        let original = cnf(&[&[1, 2], &[-1, 3]]);
        let mut w = Working::new(&original);
        let mut t = Reconstruction::new();
        eliminate_var(&mut w, var(1), &mut t);

        // Model of the residual (2 or 3): satisfy it with 3 alone, leaving 2 false.
        let mut m = Model::all_false(3);
        m.set(var(3), true);
        assert!(w.to_cnf().is_satisfied_by(&m));

        t.extend(&mut m);
        assert!(original.is_satisfied_by(&m), "reconstruction must satisfy the original");
    }

    #[test]
    fn reconstruction_handles_a_chain_of_eliminations() {
        let original = cnf(&[&[1, 2], &[-1, 3], &[-2, 4], &[-3, -4, 5]]);
        let mut w = Working::new(&original);
        let mut t = Reconstruction::new();
        for v in [1, 2, 3] {
            eliminate_var(&mut w, var(v), &mut t);
        }
        let residual = w.to_cnf();

        // Brute-force a model of the residual over the variables that remain.
        let n = residual.num_vars();
        let mut found = None;
        for mask in 0..1_u32 << n {
            let m = Model::from_values((0..n).map(|i| mask >> i & 1 == 1).collect());
            if residual.is_satisfied_by(&m) {
                found = Some(m);
                break;
            }
        }
        let mut m = found.expect("residual is satisfiable");
        t.extend(&mut m);
        assert!(original.is_satisfied_by(&m));
    }

    #[test]
    fn compacting_occurrences_preserves_answers() {
        let mut w = Working::new(&cnf(&[&[1, 2], &[1, 3], &[-1, 4]]));
        w.remove(0);
        let before = w.count(lit(1));
        w.compact_occurrences();
        assert_eq!(w.count(lit(1)), before);
        assert_eq!(w.count(lit(-1)), 1);
    }
}
