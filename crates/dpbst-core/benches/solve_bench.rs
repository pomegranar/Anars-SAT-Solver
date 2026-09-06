//! End-to-end solve benchmarks over generated instances.
//!
//! Complements `cache_bench`, which measures the memo in isolation. These run the whole solver,
//! so they answer the question that actually matters: does any of this make the solver faster?
//!
//! Instances are generated in-process rather than read from disk, so the benchmark runs without
//! having fetched the SATLIB corpus first, and so the shapes are chosen to isolate one effect
//! each:
//!
//! * `random` — a formula that never decomposes. The memo cannot help; this measures its cost.
//! * `blocks` — independent subproblems. Decomposition helps enormously; the memo still cannot,
//!   because no subproblem ever recurs.
//! * `chain` — blocks joined in a path. Both decomposition and the memo should pay.
//! * `pigeonhole` — highly connected and exponential, where the memo earns its keep by
//!   recognising subproblems reached along different branches.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use dpbst_core::cache::BucketKind;
use dpbst_core::config::{Algorithm, Config};
use dpbst_core::solver::solve_here;
use dpbst_core::{Cnf, Lit};
use std::hint::black_box;

/// xorshift64*, so instances are reproducible without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn random_3sat_into(f: &mut Cnf, rng: &mut Rng, num_vars: usize, num_clauses: usize, offset: usize) {
    let mut clause: Vec<Lit> = Vec::with_capacity(3);
    for _ in 0..num_clauses {
        clause.clear();
        while clause.len() < 3 {
            let var = (rng.next() as usize % num_vars) + offset + 1;
            let sign = if rng.next() % 2 == 0 { 1 } else { -1 };
            let lit = Lit::from_dimacs(var as i32 * sign);
            if !clause.iter().any(|&l| l.var() == lit.var()) {
                clause.push(lit);
            }
        }
        f.add_clause(&clause);
    }
}

fn random_instance(num_vars: usize) -> Cnf {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut f = Cnf::new(num_vars);
    random_3sat_into(&mut f, &mut rng, num_vars, (num_vars as f64 * 4.26) as usize, 0);
    f
}

fn block_instance(blocks: usize, per_block: usize) -> Cnf {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut f = Cnf::new(blocks * per_block);
    for b in 0..blocks {
        random_3sat_into(
            &mut f,
            &mut rng,
            per_block,
            (per_block as f64 * 4.26) as usize,
            b * per_block,
        );
    }
    f
}

fn chain_instance(blocks: usize, per_block: usize) -> Cnf {
    let mut rng = Rng(0x1357_9bdf_0246_8ace);
    let mut f = Cnf::new(blocks * per_block);
    for b in 0..blocks {
        let offset = b * per_block;
        random_3sat_into(&mut f, &mut rng, per_block, (per_block as f64 * 4.0) as usize, offset);
        if b + 1 < blocks {
            let here = (offset + per_block) as i32;
            let next = here + 1;
            f.add_dimacs_clause(&[-here, next]);
            f.add_dimacs_clause(&[here, -next]);
        }
    }
    f
}

fn pigeonhole(holes: usize) -> Cnf {
    let pigeons = holes + 1;
    let var = |p: usize, h: usize| (p * holes + h) as i32 + 1;
    let mut f = Cnf::new(pigeons * holes);
    for p in 0..pigeons {
        f.add_dimacs_clause(&(0..holes).map(|h| var(p, h)).collect::<Vec<_>>());
    }
    for h in 0..holes {
        for p in 0..pigeons {
            for q in p + 1..pigeons {
                f.add_dimacs_clause(&[-var(p, h), -var(q, h)]);
            }
        }
    }
    f
}

/// Every bucket policy, end to end, on instances where the memo is actually used.
fn bench_bucket_policies(c: &mut Criterion) {
    let instances = [("pigeonhole-9", pigeonhole(8)), ("chain-8x24", chain_instance(8, 24))];

    let mut group = c.benchmark_group("solve/bucket-policy");
    group.sample_size(20);
    for (name, formula) in &instances {
        for bucket in BucketKind::ALL {
            group.bench_with_input(BenchmarkId::new(*name, bucket), bucket, |b, &bucket| {
                let config = Config { bucket, ..Config::default() };
                b.iter(|| black_box(solve_here(formula, &config).outcome.exit_code()));
            });
        }
    }
    group.finish();
}

/// The ablation that matters: what does the memo buy, per instance shape?
fn bench_memo_ablation(c: &mut Criterion) {
    let instances = [
        ("random-120", random_instance(120)),
        ("blocks-8x30", block_instance(8, 30)),
        ("chain-8x24", chain_instance(8, 24)),
        ("pigeonhole-9", pigeonhole(8)),
    ];

    let variants: [(&str, Config); 3] = [
        ("memo", Config::default()),
        ("decompose-only", Config { cache: false, ..Config::default() }),
        ("plain-dpll", Config::plain_dpll()),
    ];

    let mut group = c.benchmark_group("solve/ablation");
    group.sample_size(20);
    for (name, formula) in &instances {
        for (variant, config) in &variants {
            group.bench_with_input(BenchmarkId::new(*name, variant), config, |b, config| {
                b.iter(|| black_box(solve_here(formula, config).outcome.exit_code()));
            });
        }
    }
    group.finish();
}

/// Davis-Putnam against DPLL, on instances small enough that resolution finishes.
fn bench_algorithms(c: &mut Criterion) {
    let instances = [("pigeonhole-6", pigeonhole(5)), ("random-40", random_instance(40))];

    let mut group = c.benchmark_group("solve/algorithm");
    group.sample_size(20);
    for (name, formula) in &instances {
        for algorithm in Algorithm::ALL {
            group.bench_with_input(BenchmarkId::new(*name, algorithm), &algorithm, |b, &algorithm| {
                let config = Config { algorithm, ..Config::default() };
                b.iter(|| black_box(solve_here(formula, &config).outcome.exit_code()));
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench_bucket_policies, bench_memo_ablation, bench_algorithms);
criterion_main!(benches);
