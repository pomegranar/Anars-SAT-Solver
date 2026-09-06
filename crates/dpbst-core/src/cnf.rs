//! Formulas in conjunctive normal form, and the assignments that satisfy them.
//!
//! Clauses live in one flat `Vec<Lit>` with a separate index of boundaries rather than as a
//! `Vec<Vec<Lit>>`. That is one allocation instead of one per clause, and it makes the whole
//! formula a contiguous scan for the preprocessor and the occurrence-list builder.

use crate::lit::{Lit, Var};
use std::fmt;

/// A formula in conjunctive normal form.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Cnf {
    num_vars: usize,
    lits: Vec<Lit>,
    /// `bounds[i]..bounds[i + 1]` is clause `i`. Always non-empty, always starts with `0`.
    bounds: Vec<u32>,
}

impl Cnf {
    /// Creates an empty formula over `num_vars` variables.
    ///
    /// The formula is trivially satisfiable until clauses are added.
    #[must_use]
    pub fn new(num_vars: usize) -> Self {
        Self {
            num_vars,
            lits: Vec::new(),
            bounds: vec![0],
        }
    }

    /// Creates an empty formula, pre-allocating for the given shape.
    #[must_use]
    pub fn with_capacity(num_vars: usize, num_clauses: usize, num_lits: usize) -> Self {
        let mut bounds = Vec::with_capacity(num_clauses + 1);
        bounds.push(0);
        Self {
            num_vars,
            lits: Vec::with_capacity(num_lits),
            bounds,
        }
    }

    /// Appends a clause verbatim, growing the variable count to cover its literals.
    ///
    /// No normalisation is performed: duplicate literals and tautologies survive. Use
    /// [`Cnf::normalized`] to clean them up.
    pub fn add_clause(&mut self, lits: &[Lit]) {
        for &l in lits {
            self.num_vars = self.num_vars.max(l.var().index() + 1);
        }
        self.lits.extend_from_slice(lits);
        self.bounds.push(self.lits.len() as u32);
    }

    /// Appends a clause given in DIMACS notation.
    ///
    /// # Panics
    /// Panics if any entry is zero.
    pub fn add_dimacs_clause(&mut self, numbers: &[i32]) {
        let lits: Vec<Lit> = numbers.iter().map(|&n| Lit::from_dimacs(n)).collect();
        self.add_clause(&lits);
    }

    /// Number of variables the formula ranges over.
    #[inline]
    #[must_use]
    pub const fn num_vars(&self) -> usize {
        self.num_vars
    }

    /// Number of clauses.
    #[inline]
    #[must_use]
    pub fn num_clauses(&self) -> usize {
        self.bounds.len() - 1
    }

    /// Total number of literal occurrences; the natural size measure for a CNF.
    #[inline]
    #[must_use]
    pub fn num_lits(&self) -> usize {
        self.lits.len()
    }

    /// Whether the formula has no clauses, and so is trivially satisfiable.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.num_clauses() == 0
    }

    /// The literals of clause `index`.
    ///
    /// # Panics
    /// Panics if `index >= self.num_clauses()`.
    #[inline]
    #[must_use]
    pub fn clause(&self, index: usize) -> &[Lit] {
        let start = self.bounds[index] as usize;
        let end = self.bounds[index + 1] as usize;
        &self.lits[start..end]
    }

    /// Iterates over the clauses in order.
    #[must_use]
    pub fn clauses(&self) -> impl ExactSizeIterator<Item = &[Lit]> {
        (0..self.num_clauses()).map(move |i| self.clause(i))
    }

    /// Raises the variable count, which is needed when a DIMACS header declares more variables
    /// than actually occur. Never lowers it.
    pub fn reserve_vars(&mut self, num_vars: usize) {
        self.num_vars = self.num_vars.max(num_vars);
    }

    /// Returns a copy with duplicate literals removed, tautologies dropped, and clauses sorted.
    ///
    /// Sorting is not cosmetic: it makes clause identity a plain slice comparison, which is what
    /// duplicate-clause removal and subsumption rely on.
    ///
    /// The flag in the returned pair is `true` if the formula contains an empty clause, i.e. it
    /// is unsatisfiable by inspection.
    #[must_use]
    pub fn normalized(&self) -> (Self, bool) {
        let mut out = Self::with_capacity(self.num_vars, self.num_clauses(), self.num_lits());
        let mut buf: Vec<Lit> = Vec::new();
        let mut has_empty = false;

        for clause in self.clauses() {
            buf.clear();
            buf.extend_from_slice(clause);
            buf.sort_unstable();
            buf.dedup();
            // After sorting, `l` and `!l` are adjacent: their encodings differ only in bit 0.
            let tautology = buf.windows(2).any(|w| w[0] == !w[1]);
            if tautology {
                continue;
            }
            if buf.is_empty() {
                has_empty = true;
            }
            out.add_clause(&buf);
        }
        out.reserve_vars(self.num_vars);
        (out, has_empty)
    }

    /// Checks a total assignment against every clause.
    #[must_use]
    pub fn is_satisfied_by(&self, model: &Model) -> bool {
        self.first_falsified_clause(model).is_none()
    }

    /// Returns the index of the first clause that `model` fails to satisfy, if any.
    ///
    /// Used by `--verify` and by the test suite, which never trusts a `SATISFIABLE` verdict
    /// without checking it against the *original* formula rather than the preprocessed one.
    #[must_use]
    pub fn first_falsified_clause(&self, model: &Model) -> Option<usize> {
        (0..self.num_clauses()).find(|&i| {
            !self
                .clause(i)
                .iter()
                .any(|&l| model.value(l.var()) == l.is_positive())
        })
    }
}

impl fmt::Debug for Cnf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Cnf({} vars, {} clauses)",
            self.num_vars,
            self.num_clauses()
        )
    }
}

impl fmt::Display for Cnf {
    /// Renders the formula as DIMACS CNF.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "p cnf {} {}", self.num_vars, self.num_clauses())?;
        for clause in self.clauses() {
            for l in clause {
                write!(f, "{} ", l.to_dimacs())?;
            }
            writeln!(f, "0")?;
        }
        Ok(())
    }
}

/// A total assignment of every variable in a formula.
///
/// Total rather than partial by construction: variables the solver never needed to assign are
/// filled with `false`, so a `Model` can always be printed as a complete DIMACS `v` line.
#[derive(Clone, PartialEq, Eq)]
pub struct Model {
    values: Vec<bool>,
}

impl Model {
    /// Creates an all-`false` assignment over `num_vars` variables.
    #[must_use]
    pub fn all_false(num_vars: usize) -> Self {
        Self {
            values: vec![false; num_vars],
        }
    }

    /// Creates a model from raw values, indexed by zero-based variable index.
    #[must_use]
    pub fn from_values(values: Vec<bool>) -> Self {
        Self { values }
    }

    /// The value assigned to `var`.
    ///
    /// # Panics
    /// Panics if `var` is outside the model.
    #[inline]
    #[must_use]
    pub fn value(&self, var: Var) -> bool {
        self.values[var.index()]
    }

    /// Sets the value of `var`.
    ///
    /// # Panics
    /// Panics if `var` is outside the model.
    #[inline]
    pub fn set(&mut self, var: Var, value: bool) {
        self.values[var.index()] = value;
    }

    /// Number of variables covered.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the model covers no variables.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The raw values, indexed by zero-based variable index.
    #[inline]
    #[must_use]
    pub fn values(&self) -> &[bool] {
        &self.values
    }

    /// The literals of the model, in variable order.
    #[must_use]
    pub fn literals(&self) -> impl ExactSizeIterator<Item = Lit> + '_ {
        self.values
            .iter()
            .enumerate()
            .map(|(i, &v)| Var::from_index(i).lit(v))
    }
}

impl fmt::Debug for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Model({} vars)", self.values.len())
    }
}

impl fmt::Display for Model {
    /// Renders the model as the body of a DIMACS `v` line, without the leading `v` or the
    /// trailing `0`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, lit) in self.literals().enumerate() {
            if i > 0 {
                write!(f, " ")?;
            }
            write!(f, "{}", lit.to_dimacs())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cnf(clauses: &[&[i32]]) -> Cnf {
        let mut f = Cnf::new(0);
        for c in clauses {
            f.add_dimacs_clause(c);
        }
        f
    }

    #[test]
    fn shape_tracks_added_clauses() {
        let f = cnf(&[&[1, -2, 3], &[-1, 2]]);
        assert_eq!(f.num_clauses(), 2);
        assert_eq!(f.num_vars(), 3);
        assert_eq!(f.num_lits(), 5);
        assert_eq!(f.clause(0), [1, -2, 3].map(Lit::from_dimacs));
        assert_eq!(f.clause(1), [-1, 2].map(Lit::from_dimacs));
    }

    #[test]
    fn normalization_drops_tautologies_and_duplicates() {
        let f = cnf(&[&[1, -1, 2], &[3, 3, -4], &[2, 1]]);
        let (n, has_empty) = f.normalized();
        assert!(!has_empty);
        // The tautology is gone, the duplicate literal collapsed.
        assert_eq!(n.num_clauses(), 2);
        assert_eq!(n.clause(0).len(), 2);
        assert_eq!(n.clause(1).len(), 2);
        // Variable count survives even though var 1 only appears in dropped positions.
        assert_eq!(n.num_vars(), 4);
    }

    #[test]
    fn normalization_reports_the_empty_clause() {
        let mut f = cnf(&[&[1]]);
        f.add_clause(&[]);
        let (_, has_empty) = f.normalized();
        assert!(has_empty);
    }

    #[test]
    fn model_checking_finds_the_falsified_clause() {
        let f = cnf(&[&[1, 2], &[-1, -2]]);
        let mut m = Model::all_false(2);
        m.set(Var::from_index(0), true);
        assert!(f.is_satisfied_by(&m));

        m.set(Var::from_index(1), true);
        // Now both are true, so (-1 or -2) fails; it is clause index 1.
        assert_eq!(f.first_falsified_clause(&m), Some(1));
        assert!(!f.is_satisfied_by(&m));
    }

    #[test]
    fn empty_formula_is_satisfied_by_anything() {
        let f = Cnf::new(3);
        assert!(f.is_empty());
        assert!(f.is_satisfied_by(&Model::all_false(3)));
    }

    #[test]
    fn display_round_trips_through_dimacs() {
        let f = cnf(&[&[1, -2], &[3]]);
        let text = f.to_string();
        assert!(text.starts_with("p cnf 3 2\n"));
        assert!(text.contains("1 -2 0\n"));
    }

    #[test]
    fn model_display_is_a_dimacs_v_line_body() {
        let m = Model::from_values(vec![true, false, true]);
        assert_eq!(m.to_string(), "1 -2 3");
    }
}
