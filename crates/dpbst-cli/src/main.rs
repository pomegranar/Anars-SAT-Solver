//! Command line front end for the `dpbst` SAT solver.
//!
//! Speaks the SAT competition dialect — `s SATISFIABLE`, `v` lines, exit codes 10 / 20 / 0 — so
//! that it drops into the same harnesses as `MiniSat`, `CaDiCaL` and Kissat with no special casing.
//! Statistics go to `stderr`, or to a machine-readable JSON line with `--json`, leaving `stdout`
//! purely DIMACS.

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser};
use dpbst_core::cache::BucketKind;
use dpbst_core::cnf::Model;
use dpbst_core::config::{Algorithm, Config};
use dpbst_core::search::heuristic::Heuristic;
use dpbst_core::solver::{Outcome, SolveResult, solve};
use dpbst_core::{Cnf, dimacs};
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// A SAT solver built on DPLL search, dynamic-programming component caching, and a hash table
/// whose buckets are binary search trees.
#[derive(Parser, Debug)]
#[command(name = "dpbst", version, about, long_about = None)]
// A CLI flag set is exactly the place where a pile of independent booleans is the right shape.
#[allow(clippy::struct_excessive_bools)]
struct Cli {
    /// DIMACS CNF file to solve. Reads standard input when omitted or given as `-`.
    input: Option<PathBuf>,

    /// Algorithm: `dpll-memo` (default), `dpll`, or `dp` for Davis and Putnam's 1960 procedure.
    #[arg(short = 'a', long, default_value_t = Algorithm::default())]
    algorithm: Algorithm,

    /// Bucket policy for the memo: `avl`, `unbalanced`, `splay`, or `chain`.
    #[arg(short = 'b', long, default_value_t = BucketKind::default())]
    bucket: BucketKind,

    /// Branching heuristic: `jw`, `dlis`, `dlcs`, `mom`, or `static`.
    #[arg(short = 'H', long, default_value_t = Heuristic::default())]
    heuristic: Heuristic,

    /// Disable the component memo, leaving plain decomposing DPLL.
    #[arg(long, action = ArgAction::SetTrue)]
    no_cache: bool,

    /// Memory budget for the memo, in mebibytes.
    #[arg(long, value_name = "MB", default_value_t = 512)]
    cache_mb: usize,

    /// Entries per bucket the memo tolerates before growing the table.
    #[arg(long, value_name = "N", default_value_t = 4)]
    target_load: usize,

    /// Disable pure literal elimination.
    #[arg(long, action = ArgAction::SetTrue)]
    no_pure_literals: bool,

    /// Disable preprocessing (bounded variable elimination and friends).
    #[arg(long, action = ArgAction::SetTrue)]
    no_preprocess: bool,

    /// Give up after this many seconds.
    #[arg(short = 't', long, value_name = "SECONDS")]
    timeout: Option<f64>,

    /// Give up after this many search nodes.
    #[arg(long, value_name = "N")]
    max_nodes: Option<u64>,

    /// Print solver statistics to standard error.
    #[arg(short = 's', long, action = ArgAction::SetTrue)]
    stats: bool,

    /// Print statistics to standard error as one JSON object, for benchmark harnesses.
    #[arg(long, action = ArgAction::SetTrue)]
    json: bool,

    /// Re-check a satisfiable model against the input and fail if it does not hold.
    #[arg(long, action = ArgAction::SetTrue)]
    verify: bool,

    /// Print only the status line, omitting the model.
    #[arg(long, action = ArgAction::SetTrue)]
    no_model: bool,
}

impl Cli {
    fn to_config(&self) -> Config {
        Config {
            algorithm: self.algorithm,
            bucket: self.bucket,
            heuristic: self.heuristic,
            cache: !self.no_cache,
            cache_budget_bytes: self.cache_mb.saturating_mul(1024 * 1024),
            target_load: self.target_load.max(1),
            pure_literals: !self.no_pure_literals,
            preprocess: !self.no_preprocess,
            timeout: self.timeout.map(Duration::from_secs_f64),
            max_nodes: self.max_nodes,
            ..Config::default()
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("dpbst: {err:#}");
            // 1 is distinct from the competition's 10 / 20 / 0, so a harness can tell a solver
            // error from a verdict.
            ExitCode::from(1)
        }
    }
}

fn run(cli: &Cli) -> Result<ExitCode> {
    let cnf = read_input(cli.input.as_deref())?;
    let config = cli.to_config();
    let result = solve(&cnf, &config);

    if cli.verify
        && let Err(clause) = result.verify(&cnf)
    {
        bail!(
            "model verification failed: clause {clause} ({:?}) is not satisfied",
            cnf.clause(clause)
        );
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    write_solution(&mut out, &result, cli.no_model)?;
    out.flush()?;

    if cli.json {
        let mut err = io::stderr().lock();
        writeln!(err, "{}", json_stats(&result, cli))?;
    } else if cli.stats {
        let mut err = io::stderr().lock();
        write_stats(&mut err, &result)?;
    }

    Ok(ExitCode::from(
        u8::try_from(result.outcome.exit_code()).expect("exit codes are 0, 10 or 20"),
    ))
}

fn read_input(path: Option<&std::path::Path>) -> Result<Cnf> {
    let parsed = match path {
        Some(p) if p != std::path::Path::new("-") => {
            let bytes = std::fs::read(p).with_context(|| format!("cannot read {}", p.display()))?;
            dimacs::parse_bytes(&bytes).with_context(|| format!("cannot parse {}", p.display()))?
        }
        _ => {
            let mut stdin = io::stdin().lock();
            if stdin.is_terminal() {
                bail!("no input file given and standard input is a terminal");
            }
            let mut bytes = Vec::new();
            stdin
                .read_to_end(&mut bytes)
                .context("cannot read standard input")?;
            dimacs::parse_bytes(&bytes).context("cannot parse standard input")?
        }
    };
    Ok(parsed.cnf)
}

/// Writes the DIMACS solution, wrapping the model across `v` lines the way `MiniSat` does.
fn write_solution<W: Write>(out: &mut W, result: &SolveResult, no_model: bool) -> io::Result<()> {
    writeln!(out, "{}", result.outcome.status_line())?;
    if no_model {
        return Ok(());
    }
    if let Outcome::Sat(model) = &result.outcome {
        write_model(out, model)?;
    }
    Ok(())
}

/// Column at which to break a `v` line. Long single-line models upset some checkers.
const MODEL_LINE_WIDTH: usize = 78;

fn write_model<W: Write>(out: &mut W, model: &Model) -> io::Result<()> {
    let mut column = 0;
    let mut line_open = false;
    for lit in model.literals() {
        let text = lit.to_dimacs().to_string();
        if !line_open || column + text.len() + 1 > MODEL_LINE_WIDTH {
            if line_open {
                writeln!(out)?;
            }
            write!(out, "v")?;
            column = 1;
            line_open = true;
        }
        write!(out, " {text}")?;
        column += text.len() + 1;
    }
    if line_open {
        writeln!(out)?;
        writeln!(out, "v 0")
    } else {
        writeln!(out, "v 0")
    }
}

fn write_stats<W: Write>(out: &mut W, result: &SolveResult) -> io::Result<()> {
    let s = &result.stats;
    writeln!(
        out,
        "c ---------------------------------------------------------------"
    )?;
    writeln!(out, "c algorithm            : {}", s.algorithm)?;
    if let Some(bucket) = s.bucket {
        writeln!(out, "c bucket policy        : {bucket}")?;
    }
    writeln!(
        out,
        "c input                : {} vars, {} clauses",
        s.input_vars, s.input_clauses
    )?;

    if let Some(p) = &s.preprocess {
        writeln!(
            out,
            "c preprocessing        : {} -> {} clauses ({} dup, {} unit, {} pure, {} eliminated)",
            p.clauses_before,
            p.clauses_after,
            p.duplicates_removed,
            p.units_fixed,
            p.pure_fixed,
            p.vars_eliminated
        )?;
    }

    if let Some(search) = &s.search {
        writeln!(out, "c search nodes         : {}", search.nodes)?;
        writeln!(out, "c decisions            : {}", search.decisions)?;
        writeln!(out, "c conflicts            : {}", search.conflicts)?;
        writeln!(out, "c propagations         : {}", search.propagations)?;
        writeln!(out, "c components           : {}", search.components)?;
        writeln!(out, "c pure literals        : {}", search.pure_literals)?;
        writeln!(out, "c max depth            : {}", search.max_depth)?;

        let c = &search.cache;
        writeln!(
            out,
            "c memo                 : {} lookups, {} hits ({:.1}%), {} entries",
            c.lookups,
            c.hits,
            c.hit_rate() * 100.0,
            search.table.entries
        )?;
        writeln!(
            out,
            "c memo maintenance     : {} inserts, {} evicted, {} sweeps, {} resizes",
            c.inserts, c.evicted, c.sweeps, c.resizes
        )?;
        writeln!(
            out,
            "c memo table           : {} buckets, {} occupied, max bucket {}, \
             max depth {}, mean depth {:.2}",
            search.table.buckets,
            search.table.occupied,
            search.table.max_bucket,
            search.table.max_depth,
            search.table.mean_depth
        )?;
        writeln!(
            out,
            "c memo memory          : {:.2} MiB",
            search.cache_bytes as f64 / (1024.0 * 1024.0)
        )?;
    }

    if let Some(dp) = &s.dp {
        writeln!(out, "c variables eliminated : {}", dp.eliminated)?;
        writeln!(out, "c resolvents           : {}", dp.resolvents)?;
        writeln!(out, "c peak clauses         : {}", dp.peak_clauses)?;
    }

    writeln!(
        out,
        "c cpu time             : {:.4} s",
        s.elapsed.as_secs_f64()
    )?;
    writeln!(
        out,
        "c ---------------------------------------------------------------"
    )
}

/// Renders the run as one JSON object.
///
/// Hand-written rather than pulling in `serde`: the shape is fixed, small, and entirely numeric,
/// and the only consumer is `scripts/bench.py`.
fn json_stats(result: &SolveResult, cli: &Cli) -> String {
    let s = &result.stats;
    let status = match result.outcome {
        Outcome::Sat(_) => "SAT",
        Outcome::Unsat => "UNSAT",
        Outcome::Unknown(_) => "UNKNOWN",
    };

    let mut json = String::from("{");
    let field = |json: &mut String, key: &str, value: String| {
        if json.len() > 1 {
            json.push(',');
        }
        json.push_str(&format!("\"{key}\":{value}"));
    };

    field(&mut json, "status", format!("\"{status}\""));
    field(&mut json, "algorithm", format!("\"{}\"", s.algorithm));
    field(&mut json, "bucket", format!("\"{}\"", cli.bucket));
    field(&mut json, "heuristic", format!("\"{}\"", cli.heuristic));
    field(&mut json, "cache", (!cli.no_cache).to_string());
    field(
        &mut json,
        "elapsed_s",
        format!("{:.6}", s.elapsed.as_secs_f64()),
    );
    field(&mut json, "input_vars", s.input_vars.to_string());
    field(&mut json, "input_clauses", s.input_clauses.to_string());

    if let Some(p) = &s.preprocess {
        field(&mut json, "pre_clauses_after", p.clauses_after.to_string());
        field(
            &mut json,
            "pre_vars_eliminated",
            p.vars_eliminated.to_string(),
        );
    }
    if let Some(search) = &s.search {
        field(&mut json, "nodes", search.nodes.to_string());
        field(&mut json, "decisions", search.decisions.to_string());
        field(&mut json, "conflicts", search.conflicts.to_string());
        field(&mut json, "propagations", search.propagations.to_string());
        field(&mut json, "components", search.components.to_string());
        field(&mut json, "max_depth", search.max_depth.to_string());
        field(&mut json, "cache_lookups", search.cache.lookups.to_string());
        field(&mut json, "cache_hits", search.cache.hits.to_string());
        field(
            &mut json,
            "cache_hit_rate",
            format!("{:.6}", search.cache.hit_rate()),
        );
        field(&mut json, "cache_entries", search.table.entries.to_string());
        field(&mut json, "cache_evicted", search.cache.evicted.to_string());
        field(&mut json, "table_buckets", search.table.buckets.to_string());
        field(
            &mut json,
            "table_max_bucket",
            search.table.max_bucket.to_string(),
        );
        field(
            &mut json,
            "table_max_depth",
            search.table.max_depth.to_string(),
        );
        field(
            &mut json,
            "table_mean_depth",
            format!("{:.6}", search.table.mean_depth),
        );
        field(&mut json, "cache_bytes", search.cache_bytes.to_string());
    }
    if let Some(dp) = &s.dp {
        field(&mut json, "dp_eliminated", dp.eliminated.to_string());
        field(&mut json, "dp_resolvents", dp.resolvents.to_string());
        field(&mut json, "dp_peak_clauses", dp.peak_clauses.to_string());
    }
    json.push('}');
    json
}
