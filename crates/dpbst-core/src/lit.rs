//! Variables and literals.
//!
//! Both are `#[repr(transparent)]` newtypes over `u32`, indexed from zero internally and
//! converted to/from DIMACS' one-based signed convention only at the I/O boundary. This split
//! is borrowed from `varisat`; keeping the one-based form out of the solver removes an entire
//! class of off-by-one bug.

use std::fmt;
use std::ops::Not;

/// A Boolean variable, identified by a zero-based index.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Var(u32);

impl Var {
    /// Largest representable variable index.
    ///
    /// Two bits of headroom are reserved below `u32::MAX`: one is consumed by the sign bit of
    /// [`Lit`], and one is kept spare so that a literal index plus a tag still fits in a word.
    pub const MAX_INDEX: usize = (u32::MAX >> 2) as usize;

    /// Creates a variable from a zero-based index.
    ///
    /// # Panics
    /// Panics if `index > Var::MAX_INDEX`.
    #[inline]
    #[must_use]
    pub fn from_index(index: usize) -> Self {
        assert!(index <= Self::MAX_INDEX, "variable index {index} out of range");
        Self(index as u32)
    }

    /// The zero-based index of this variable.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// The one-based, positive DIMACS number for this variable.
    #[inline]
    #[must_use]
    pub const fn to_dimacs(self) -> i32 {
        self.0 as i32 + 1
    }

    /// Builds a literal over this variable with the given polarity.
    #[inline]
    #[must_use]
    pub const fn lit(self, positive: bool) -> Lit {
        Lit(self.0 << 1 | !positive as u32)
    }

    /// The positive literal over this variable.
    #[inline]
    #[must_use]
    pub const fn positive(self) -> Lit {
        self.lit(true)
    }

    /// The negative literal over this variable.
    #[inline]
    #[must_use]
    pub const fn negative(self) -> Lit {
        self.lit(false)
    }
}

impl fmt::Debug for Var {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "x{}", self.0)
    }
}

/// A literal: a variable together with a polarity.
///
/// Encoded as `2 * var + negated`, the `MiniSat` convention. Two useful properties follow:
/// negation is `^ 1`, and a literal doubles as an index into a `2 * num_vars` array, which is
/// how occurrence lists are laid out.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Lit(u32);

impl Lit {
    /// Builds a literal from a variable and a polarity.
    #[inline]
    #[must_use]
    pub const fn new(var: Var, positive: bool) -> Self {
        var.lit(positive)
    }

    /// Builds a literal from a non-zero DIMACS number.
    ///
    /// # Panics
    /// Panics if `number` is zero or its magnitude exceeds [`Var::MAX_INDEX`].
    #[inline]
    #[must_use]
    pub fn from_dimacs(number: i32) -> Self {
        assert!(number != 0, "0 is not a DIMACS literal");
        let var = Var::from_index(number.unsigned_abs() as usize - 1);
        var.lit(number > 0)
    }

    /// The signed DIMACS number for this literal.
    #[inline]
    #[must_use]
    pub const fn to_dimacs(self) -> i32 {
        let var = (self.0 >> 1) as i32 + 1;
        if self.is_positive() { var } else { -var }
    }

    /// The variable underlying this literal.
    #[inline]
    #[must_use]
    pub const fn var(self) -> Var {
        Var(self.0 >> 1)
    }

    /// Whether this literal is the variable rather than its negation.
    #[inline]
    #[must_use]
    pub const fn is_positive(self) -> bool {
        self.0 & 1 == 0
    }

    /// A dense index in `0..2 * num_vars`, suitable for array lookup.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Reconstructs a literal from [`Lit::index`].
    #[inline]
    #[must_use]
    pub const fn from_index(index: usize) -> Self {
        Self(index as u32)
    }
}

impl Not for Lit {
    type Output = Self;

    /// Negation is a single bit flip, which is the whole point of the encoding.
    #[inline]
    fn not(self) -> Self {
        Self(self.0 ^ 1)
    }
}

impl fmt::Debug for Lit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_dimacs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimacs_round_trip() {
        for n in [1i32, 2, 7, -1, -2, -12345] {
            let lit = Lit::from_dimacs(n);
            assert_eq!(lit.to_dimacs(), n);
            assert_eq!(lit.var().to_dimacs(), n.abs());
            assert_eq!(lit.is_positive(), n > 0);
        }
    }

    #[test]
    fn negation_is_involutive_and_shares_a_var() {
        let l = Lit::from_dimacs(9);
        assert_eq!(!!l, l);
        assert_ne!(!l, l);
        assert_eq!((!l).var(), l.var());
        assert_eq!(l.index() ^ 1, (!l).index());
    }

    #[test]
    fn index_round_trip() {
        let l = Lit::from_dimacs(-42);
        assert_eq!(Lit::from_index(l.index()), l);
    }

    #[test]
    fn polarity_constructors_agree() {
        let v = Var::from_index(3);
        assert_eq!(v.positive(), Lit::new(v, true));
        assert_eq!(v.negative(), !v.positive());
        assert_eq!(v.positive().to_dimacs(), 4);
    }

    #[test]
    #[should_panic(expected = "0 is not a DIMACS literal")]
    fn zero_literal_rejected() {
        let _ = Lit::from_dimacs(0);
    }
}
