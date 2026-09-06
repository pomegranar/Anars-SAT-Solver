//! `dpbst` — a SAT solver built on DPLL search, dynamic-programming component caching, and a hash
//! table whose buckets are binary search trees.
//!
//! # What it does
//!
//! Given a Boolean formula in conjunctive normal form, decide whether some assignment of its
//! variables makes it true, and produce one if so.
//!
//! # How
//!
//! Ordinary DPLL backtracking search, plus two things:
//!
//! * **Decomposition.** After some variables are assigned, the clauses that remain often fall
//!   into groups sharing no variables. Those groups are solved independently.
//! * **Memoisation.** Each group is named canonically and its verdict remembered, so a subproblem
//!   reached twice by different routes is solved once. That memo is the hash-table-of-BSTs.
//!
//! ```
//! use dpbst_core::{Cnf, config::Config, solver::{solve, Outcome}};
//!
//! let mut formula = Cnf::new(0);
//! formula.add_dimacs_clause(&[1, 2]);
//! formula.add_dimacs_clause(&[-1, 2]);
//! formula.add_dimacs_clause(&[-2]);
//!
//! match solve(&formula, &Config::default()).outcome {
//!     Outcome::Unsat => println!("no assignment works"),
//!     Outcome::Sat(model) => println!("found: {model}"),
//!     Outcome::Unknown(reason) => println!("gave up: {reason}"),
//! }
//! ```

pub mod cache;
pub mod cnf;
pub mod config;
pub mod dimacs;
pub mod dp;
pub mod elim;
pub mod lit;
pub mod preprocess;
pub mod search;
pub mod solver;
pub mod varint;

pub use cnf::{Cnf, Model};
pub use config::{Algorithm, Config};
pub use lit::{Lit, Var};
pub use solver::{AbortReason, Outcome, SolveResult, solve};
