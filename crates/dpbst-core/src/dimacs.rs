//! DIMACS CNF reading and writing.
//!
//! A hand-rolled byte scanner rather than line splitting plus `str::parse`: benchmark files run
//! to tens of megabytes and the parser should not be the thing that shows up in a profile.
//!
//! Two real-world quirks are handled deliberately, because most of the SATLIB corpus trips at
//! least one of them:
//!
//! * SATLIB instances terminate the clause list with a `%` line followed by a stray `0`. Parsing
//!   past it yields a spurious empty clause and a confidently wrong `UNSATISFIABLE`.
//! * Headers routinely disagree with the body. The header is treated as an allocation hint and
//!   a cross-check, never as ground truth.

use crate::cnf::Cnf;
use crate::lit::{Lit, Var};
use std::io::{self, Read, Write};

/// Something wrong with a DIMACS input.
#[derive(Debug, thiserror::Error)]
pub enum DimacsError {
    #[error("line {line}: expected the header to read `p cnf <vars> <clauses>`")]
    BadHeader { line: usize },
    #[error("line {line}: `{token}` is not an integer")]
    BadInteger { line: usize, token: String },
    #[error("line {line}: literal {value} refers to variable {var}, above the supported maximum")]
    VarTooLarge { line: usize, value: i64, var: u64 },
    #[error("input ends in the middle of a clause; a trailing `0` is missing")]
    UnterminatedClause,
    #[error("no `p cnf` header found")]
    MissingHeader,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Outcome of a parse, including what the header claimed.
#[derive(Debug, Clone)]
pub struct ParsedCnf {
    /// The formula as read.
    pub cnf: Cnf,
    /// Variable count from the `p cnf` header, if there was one.
    pub declared_vars: Option<usize>,
    /// Clause count from the `p cnf` header, if there was one.
    pub declared_clauses: Option<usize>,
}

impl ParsedCnf {
    /// Whether the header's counts match what was actually read.
    ///
    /// A mismatch is not an error — plenty of real instances have one — but it is worth
    /// reporting under `--stats`.
    #[must_use]
    pub fn header_matches_body(&self) -> bool {
        self.declared_clauses
            .is_none_or(|c| c == self.cnf.num_clauses())
            && self.declared_vars.is_none_or(|v| v == self.cnf.num_vars())
    }
}

/// Parses DIMACS CNF from a byte slice.
pub fn parse_bytes(input: &[u8]) -> Result<ParsedCnf, DimacsError> {
    Parser::new(input).run()
}

/// Reads DIMACS CNF from any reader, buffering the whole input first.
pub fn parse_reader<R: Read>(mut reader: R) -> Result<ParsedCnf, DimacsError> {
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;
    parse_bytes(&buf)
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    line: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            line: 1,
        }
    }

    fn run(mut self) -> Result<ParsedCnf, DimacsError> {
        let mut declared_vars = None;
        let mut declared_clauses = None;
        let mut cnf: Option<Cnf> = None;

        // Header and comments may be interleaved; scan until the first non-comment content.
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'c') => self.skip_line(),
                Some(b'p') => {
                    let (vars, clauses) = self.parse_header()?;
                    declared_vars = Some(vars);
                    declared_clauses = Some(clauses);
                    // Trust the header enough to size the allocation. An average clause in the
                    // wild is short, so three literals is a reasonable guess for the literal pool.
                    cnf = Some(Cnf::with_capacity(vars, clauses, clauses.saturating_mul(3)));
                    break;
                }
                // End of input, SATLIB's `%` marker, or the first clause: header phase over.
                _ => break,
            }
        }

        let mut cnf = match cnf {
            Some(c) => c,
            // A missing header is tolerated; some generators omit it.
            None => Cnf::new(0),
        };

        let mut clause: Vec<Lit> = Vec::with_capacity(8);
        loop {
            self.skip_whitespace();
            match self.peek() {
                // End of input, or SATLIB's `%` marker: everything after it is not clause data.
                None | Some(b'%') => break,
                Some(b'c') => {
                    self.skip_line();
                    continue;
                }
                _ => {}
            }

            let value = self.parse_int()?;
            if value == 0 {
                cnf.add_clause(&clause);
                clause.clear();
            } else {
                let magnitude = value.unsigned_abs();
                if magnitude > Var::MAX_INDEX as u64 {
                    return Err(DimacsError::VarTooLarge {
                        line: self.line,
                        value,
                        var: magnitude,
                    });
                }
                let var = Var::from_index(magnitude as usize - 1);
                clause.push(var.lit(value > 0));
            }
        }

        if !clause.is_empty() {
            return Err(DimacsError::UnterminatedClause);
        }
        if let Some(v) = declared_vars {
            cnf.reserve_vars(v);
        }

        Ok(ParsedCnf {
            cnf,
            declared_vars,
            declared_clauses,
        })
    }

    fn parse_header(&mut self) -> Result<(usize, usize), DimacsError> {
        let line = self.line;
        self.pos += 1; // consume 'p'
        self.skip_spaces();
        if !self.consume_word(b"cnf") {
            return Err(DimacsError::BadHeader { line });
        }
        let vars = self.parse_int()?;
        let clauses = self.parse_int()?;
        if vars < 0 || clauses < 0 {
            return Err(DimacsError::BadHeader { line });
        }
        Ok((vars as usize, clauses as usize))
    }

    fn consume_word(&mut self, word: &[u8]) -> bool {
        if self.input[self.pos..].starts_with(word) {
            self.pos += word.len();
            true
        } else {
            false
        }
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    /// Skips spaces and tabs but stops at a newline, so the line counter stays honest.
    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r')) {
            self.pos += 1;
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b'\n' => {
                    self.line += 1;
                    self.pos += 1;
                }
                b' ' | b'\t' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn skip_line(&mut self) {
        while let Some(b) = self.peek() {
            self.pos += 1;
            if b == b'\n' {
                self.line += 1;
                break;
            }
        }
    }

    /// Parses one signed integer, in `i64` so that an out-of-range variable can be reported as a
    /// clear diagnostic rather than silently wrapping.
    fn parse_int(&mut self) -> Result<i64, DimacsError> {
        self.skip_whitespace();
        let start = self.pos;
        let negative = match self.peek() {
            Some(b'-') => {
                self.pos += 1;
                true
            }
            Some(b'+') => {
                self.pos += 1;
                false
            }
            _ => false,
        };

        let digits_start = self.pos;
        let mut value: i64 = 0;
        let mut overflow = false;
        while let Some(b @ b'0'..=b'9') = self.peek() {
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(i64::from(b - b'0')))
                .unwrap_or_else(|| {
                    overflow = true;
                    i64::MAX
                });
            self.pos += 1;
        }

        if self.pos == digits_start || overflow {
            // Consume the rest of the offending token so the message shows all of it.
            while matches!(self.peek(), Some(b) if !b.is_ascii_whitespace()) {
                self.pos += 1;
            }
            let token = String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
            return Err(DimacsError::BadInteger {
                line: self.line,
                token,
            });
        }

        Ok(if negative { -value } else { value })
    }
}

/// Writes a formula as DIMACS CNF.
pub fn write_cnf<W: Write>(mut out: W, cnf: &Cnf) -> io::Result<()> {
    write!(out, "{cnf}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Cnf {
        parse_bytes(text.as_bytes()).expect("should parse").cnf
    }

    #[test]
    fn parses_a_plain_instance() {
        let cnf = parse("p cnf 3 2\n1 -2 0\n2 3 0\n");
        assert_eq!(cnf.num_vars(), 3);
        assert_eq!(cnf.num_clauses(), 2);
        assert_eq!(cnf.clause(0), [1, -2].map(Lit::from_dimacs));
    }

    #[test]
    fn comments_are_ignored_anywhere() {
        let cnf = parse("c leading\nc more\np cnf 2 2\n1 0\nc between\n-2 0\n");
        assert_eq!(cnf.num_clauses(), 2);
    }

    #[test]
    fn clauses_may_span_lines_and_share_them() {
        let cnf = parse("p cnf 4 2\n1\n-2 0 3 4\n0\n");
        assert_eq!(cnf.num_clauses(), 2);
        assert_eq!(cnf.clause(0).len(), 2);
        assert_eq!(cnf.clause(1).len(), 2);
    }

    /// The quirk that matters: SATLIB files end with `%` then a lone `0`. Reading through it
    /// invents an empty clause and turns every instance unsatisfiable.
    #[test]
    fn satlib_percent_terminator_stops_the_parse() {
        let cnf = parse("p cnf 2 1\n1 2 0\n%\n0\n\n");
        assert_eq!(cnf.num_clauses(), 1);
        assert!(cnf.clauses().all(|c| !c.is_empty()));
    }

    #[test]
    fn header_is_a_hint_not_ground_truth() {
        let parsed = parse_bytes(b"p cnf 10 5\n1 0\n").unwrap();
        assert_eq!(parsed.declared_vars, Some(10));
        assert_eq!(parsed.declared_clauses, Some(5));
        assert_eq!(parsed.cnf.num_clauses(), 1);
        assert!(!parsed.header_matches_body());
        // The declared variable count still widens the formula.
        assert_eq!(parsed.cnf.num_vars(), 10);
    }

    #[test]
    fn missing_header_is_tolerated() {
        let cnf = parse("1 -2 0\n2 0\n");
        assert_eq!(cnf.num_clauses(), 2);
        assert_eq!(cnf.num_vars(), 2);
    }

    #[test]
    fn empty_clause_is_preserved() {
        let cnf = parse("p cnf 1 2\n1 0\n0\n");
        assert_eq!(cnf.num_clauses(), 2);
        assert!(cnf.clause(1).is_empty());
    }

    #[test]
    fn unterminated_clause_is_an_error() {
        let err = parse_bytes(b"p cnf 2 1\n1 2\n").unwrap_err();
        assert!(matches!(err, DimacsError::UnterminatedClause));
    }

    #[test]
    fn garbage_token_is_reported_with_its_line() {
        let err = parse_bytes(b"p cnf 2 1\n1 two 0\n").unwrap_err();
        match err {
            DimacsError::BadInteger { line, token } => {
                assert_eq!(line, 2);
                assert_eq!(token, "two");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn bad_header_keyword_is_rejected() {
        let err = parse_bytes(b"p dnf 2 1\n1 0\n").unwrap_err();
        assert!(matches!(err, DimacsError::BadHeader { line: 1 }));
    }

    #[test]
    fn oversized_variable_is_rejected_rather_than_wrapping() {
        let err = parse_bytes(b"p cnf 1 1\n9999999999 0\n").unwrap_err();
        assert!(matches!(err, DimacsError::VarTooLarge { .. }));
    }

    #[test]
    fn writing_then_parsing_is_the_identity() {
        let original = parse("p cnf 3 2\n1 -2 0\n-3 0\n");
        let mut buf = Vec::new();
        write_cnf(&mut buf, &original).unwrap();
        let round_tripped = parse_bytes(&buf).unwrap().cnf;
        assert_eq!(original, round_tripped);
    }
}
